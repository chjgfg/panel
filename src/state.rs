// 全局共享状态：配置、会话表、各类采样缓存、启动参数的落盘。
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::hostinfo::Disk;

pub const SESSION_SECS: u64 = 7 * 86400;
const MAX_FAILS: u32 = 5;
const LOCK_SECS: u64 = 600;
/// 同时最多开几路网页控制台。每路都是一个 PTY + 一个 ssh 子进程，
/// 单核机器上开十几路就是白送的资源和文件描述符。
pub const MAX_TERMS: usize = 4;

/// 取锁：就算锁被前一次 panic 弄中毒了，也把里面的值拿出来继续用。
/// 用 lock().unwrap() 的话，一次 panic 恰好发生在持锁期间，之后每一次
/// lock().unwrap() 都会 panic —— 面板还在监听端口，但每个请求都立刻挂掉，
/// 只能重启进程救回来。这里换来的代价是「可能读到一份改到一半的缓存」，
/// 而这些字段全是会话表、采样缓存、启动参数，下一轮就会自愈，轻得多。
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct App {
    pub cfg: Config,
    /// cargo 的绝对路径，启动时定好
    pub cargo: String,
    /// 网页控制台用的 SSH 私钥落盘路径（跟配置文件同目录的 panel_ssh_key）。
    /// 存服务器上而不是浏览器里，换个浏览器也不用重配。文件权限 0600。
    pub ssh_key_path: PathBuf,
    /// 启动参数落盘路径（配置文件同目录的 panel_state.toml）
    pub state_path: PathBuf,
    /// token -> 过期时刻
    pub sessions: Mutex<HashMap<String, Instant>>,
    /// (连续失败次数, 最后一次失败时刻)
    pub fails: Mutex<(u32, Instant)>,
    /// unit -> (上次读到的 CPU 累计纳秒, 采样时刻)
    pub cpu_prev: Mutex<HashMap<String, (u64, Instant)>>,
    /// 项目名 -> 最近一次启动的那组 (bin, 参数)。「重启」要沿用它把整组重新拉起来，
    /// 所以要记一组而不是一个 bin。这份同时会落盘，见 save_last。
    pub last: Mutex<HashMap<String, Vec<(String, String)>>>,
    /// 整机 CPU 的上次采样 (忙碌时间片, 总时间片)
    pub host_cpu: Mutex<Option<(u64, u64)>>,
    /// df 的结果缓存。磁盘占用变化很慢，没必要每 3 秒 fork 一个 df
    pub disks: Mutex<Option<(Instant, Vec<Disk>)>>,
    /// 当前开着的控制台路数，terminal.rs 进出时加减
    pub terms: AtomicUsize,
}

/// 落盘格式。用 struct 而不是裸 map，是为了以后要加字段时旧文件还能读
#[derive(Serialize, Deserialize, Default)]
struct SavedState {
    #[serde(default)]
    entries: Vec<SavedEntry>,
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    project: String,
    bin: String,
    args: String,
}

impl App {
    pub fn new(
        cfg: Config,
        cargo: String,
        ssh_key_path: PathBuf,
        state_path: PathBuf,
    ) -> Arc<App> {
        let app = Arc::new(App {
            cfg,
            cargo,
            ssh_key_path,
            state_path,
            sessions: Mutex::new(HashMap::new()),
            fails: Mutex::new((0, Instant::now())),
            cpu_prev: Mutex::new(HashMap::new()),
            last: Mutex::new(HashMap::new()),
            host_cpu: Mutex::new(None),
            disks: Mutex::new(None),
            terms: AtomicUsize::new(0),
        });
        app.load_last();
        app
    }

    /// 读回上次那组 (bin, 参数)。文件不存在、读不动、格式不认识都当「没记过」，
    /// 不能因为一个缓存文件把面板卡住起不来。
    fn load_last(&self) {
        let Ok(text) = std::fs::read_to_string(&self.state_path) else {
            return;
        };
        match toml::from_str::<SavedState>(&text) {
            Ok(s) => {
                let mut m: HashMap<String, Vec<(String, String)>> = HashMap::new();
                for e in s.entries {
                    m.entry(e.project).or_default().push((e.bin, e.args));
                }
                let n: usize = m.values().map(Vec::len).sum();
                *lock(&self.last) = m;
                if n > 0 {
                    println!("读回上次启动参数: {n} 条（{p}）", p = self.state_path.display());
                }
            }
            Err(e) => eprintln!("提示：启动参数文件 {} 读不懂，忽略（{e}）", self.state_path.display()),
        }
    }

    /// 把 last 写回磁盘。spawn 成功之后调一次，内容就几行，不值得做增量。
    /// 写失败不影响本次启动，只是下次重启恢复不了，所以只打一行提示。
    pub fn save_last(&self) {
        // 项目名排序（同样内容写出同样的文件，看 diff 不噪音），但一个项目内部
        // 保持原顺序 —— 界面回显「上次用的是哪个 bin」取的是第一个，打乱就错了
        let mut snapshot: Vec<(String, Vec<(String, String)>)> = {
            lock(&self.last).iter().map(|(p, g)| (p.clone(), g.clone())).collect()
        };
        snapshot.sort_by(|a, b| a.0.cmp(&b.0));
        let entries: Vec<SavedEntry> = snapshot
            .into_iter()
            .flat_map(|(project, group)| {
                group.into_iter().map(move |(bin, args)| SavedEntry {
                    project: project.clone(),
                    bin,
                    args,
                })
            })
            .collect();
        let text = match toml::to_string_pretty(&SavedState { entries }) {
            Ok(t) => t,
            Err(e) => return eprintln!("提示：启动参数序列化失败，没落盘（{e}）"),
        };
        if let Err(e) = std::fs::write(&self.state_path, text) {
            eprintln!("提示：启动参数写不进 {}（{e}）", self.state_path.display());
        }
    }

    /// 占一路控制台额度。满了返回 false，调用方直接回 503
    pub fn take_term(&self) -> bool {
        loop {
            let n = self.terms.load(Ordering::Relaxed);
            if n >= MAX_TERMS {
                return false;
            }
            if self
                .terms
                .compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn drop_term(&self) {
        self.terms.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn valid_session(&self, tok: &str) -> bool {
        let mut s = lock(&self.sessions);
        let now = Instant::now();
        s.retain(|_, exp| *exp > now);
        s.contains_key(tok)
    }

    pub fn new_session(&self) -> Option<String> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).ok()?;
        let tok: String = b.iter().map(|x| format!("{x:02x}")).collect();
        lock(&self.sessions).insert(
            tok.clone(),
            Instant::now() + Duration::from_secs(SESSION_SECS),
        );
        Some(tok)
    }

    /// 连续失败 MAX_FAILS 次就锁 LOCK_SECS 秒。挡的是自动化撞库：
    /// 密码接口不限速的话，攻击者能靠并发每秒试几千次。
    /// 注意登录时是「先验密码、再看不看脸色」（见 auth::login），
    /// 所以这个锁只拦猜错的人，不会把拿对密码的你关在自己门外。
    pub fn lockout(&self) -> Option<u64> {
        let (n, last) = *lock(&self.fails);
        if n < MAX_FAILS {
            return None;
        }
        let e = last.elapsed().as_secs();
        (e < LOCK_SECS).then(|| LOCK_SECS - e)
    }

    pub fn on_fail(&self) {
        let mut f = lock(&self.fails);
        if f.0 >= MAX_FAILS && f.1.elapsed().as_secs() >= LOCK_SECS {
            *f = (0, Instant::now()); // 锁定期已过，重新计数
        }
        f.0 += 1;
        f.1 = Instant::now();
    }

    /// 登录成功后清掉失败计数
    pub fn reset_fails(&self) {
        *lock(&self.fails) = (0, Instant::now());
    }

    /// CPUUsageNSec 是开机以来的累计值，两次采样做差才是占用率。100% = 吃满一个核
    pub fn cpu(&self, unit: &str, cur: Option<u64>) -> Option<f64> {
        let cur = cur?;
        let now = Instant::now();
        let (prev, t) = lock(&self.cpu_prev).insert(unit.to_string(), (cur, now))?;
        let dt = now.duration_since(t).as_secs_f64();
        // 首次采样、间隔过短、或进程重启导致计数器归零，都算不出有意义的值
        (dt >= 0.5 && cur >= prev).then(|| (cur - prev) as f64 / 1e9 / dt * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("panel-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 只用于测试的 App：状态文件指到临时路径，不碰真配置旁边那个
    fn test_app(state_path: PathBuf) -> App {
        App {
            cfg: Config {
                bind: "127.0.0.1:0".into(),
                password: "x".repeat(12),
                dirs: vec!["/tmp".into()],
                exclude: vec![],
                cargo: None,
                prefix: String::new(),
            },
            cargo: "/usr/bin/cargo".into(),
            ssh_key_path: tmp("appkey"),   // 别叫 "key"：跟 terminal.rs 的临时文件测试撞名
            state_path,
            sessions: Mutex::new(HashMap::new()),
            fails: Mutex::new((0, Instant::now())),
            cpu_prev: Mutex::new(HashMap::new()),
            last: Mutex::new(HashMap::new()),
            host_cpu: Mutex::new(None),
            disks: Mutex::new(None),
            terms: AtomicUsize::new(0),
        }
    }

    #[test]
    fn last_survives_roundtrip() {
        // 落盘再读回：模拟「面板/服务器重启后，一键恢复还得知道上次起了什么」
        let path = tmp("state");
        let app = test_app(path.clone());
        lock(&app.last).insert(
            "xau".into(),
            vec![
                ("web".into(), "--port 8080".into()),
                ("shell".into(), "".into()),
            ],
        );
        app.save_last();
        assert!(path.is_file(), "应该写出一个状态文件");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("xau") && text.contains("--port 8080"), "{text}");

        let app2 = test_app(path.clone());
        app2.load_last();
        let got = lock(&app2.last).get("xau").cloned().unwrap_or_default();
        assert_eq!(
            got,
            vec![
                ("web".into(), "--port 8080".into()),
                ("shell".into(), "".into())
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unreadable_state_file_is_ignored() {
        // 状态文件坏了不能把面板卡住：读回时静默忽略，照样起得来
        let path = tmp("badstate");
        std::fs::write(&path, "这不是 toml ][ 乱码").unwrap();
        let app = test_app(path.clone());
        app.load_last();
        assert!(lock(&app.last).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn term_slots_capped_and_released() {
        let app = test_app(tmp("state"));
        for _ in 0..MAX_TERMS {
            assert!(app.take_term(), "没满之前都要给额度");
        }
        assert!(!app.take_term(), "满了要拒");
        app.drop_term();
        assert!(app.take_term(), "放掉一路后要能再进");
    }

    #[test]
    fn poisoned_lock_still_yields_its_value() {
        // 持锁期间 panic 一次，锁就中毒：unwrap() 之后会一直 panic，
        // lock() 必须照样给值，否则面板就成了还在监听的一具尸体
        let m = Arc::new(Mutex::new(7u32));
        let h = m.clone();
        let t = std::thread::spawn(move || {
            let mut g = h.lock().unwrap();
            *g = 9;
            panic!("持锁期间炸一下");
        });
        let _ = t.join(); // 子线程的 panic 收在这儿，不让它弄挂测试
        assert!(m.is_poisoned(), "前提：锁确实中毒了");
        assert_eq!(*lock(&m), 9, "中毒也要拿到值");
    }
}
