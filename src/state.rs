// 全局共享状态：配置、会话表、各类采样缓存。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::hostinfo::Disk;

pub const SESSION_SECS: u64 = 7 * 86400;
const MAX_FAILS: u32 = 5;
const LOCK_SECS: u64 = 600;

pub struct App {
    pub cfg: Config,
    /// cargo 的绝对路径，启动时定好
    pub cargo: String,
    /// token -> 过期时刻
    pub sessions: Mutex<HashMap<String, Instant>>,
    /// (连续失败次数, 最后一次失败时刻)
    pub fails: Mutex<(u32, Instant)>,
    /// unit -> (上次读到的 CPU 累计纳秒, 采样时刻)
    pub cpu_prev: Mutex<HashMap<String, (u64, Instant)>>,
    /// 项目名 -> 最近一次启动的那组 (bin, 参数)。「重启」要沿用它把整组重新拉起来，
    /// 所以要记一组而不是一个 bin。
    pub last: Mutex<HashMap<String, Vec<(String, String)>>>,
    /// 整机 CPU 的上次采样 (忙碌时间片, 总时间片)
    pub host_cpu: Mutex<Option<(u64, u64)>>,
    /// df 的结果缓存。磁盘占用变化很慢，没必要每 3 秒 fork 一个 df
    pub disks: Mutex<Option<(Instant, Vec<Disk>)>>,
}

impl App {
    pub fn new(cfg: Config, cargo: String) -> Arc<Self> {
        Arc::new(App {
            cfg,
            cargo,
            sessions: Mutex::new(HashMap::new()),
            fails: Mutex::new((0, Instant::now())),
            cpu_prev: Mutex::new(HashMap::new()),
            last: Mutex::new(HashMap::new()),
            host_cpu: Mutex::new(None),
            disks: Mutex::new(None),
        })
    }

    pub fn valid_session(&self, tok: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, exp| *exp > now);
        s.contains_key(tok)
    }

    pub fn new_session(&self) -> Option<String> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).ok()?;
        let tok: String = b.iter().map(|x| format!("{x:02x}")).collect();
        self.sessions.lock().unwrap().insert(
            tok.clone(),
            Instant::now() + Duration::from_secs(SESSION_SECS),
        );
        Some(tok)
    }

    /// 连续失败 MAX_FAILS 次就锁 LOCK_SECS 秒。挡的是自动化撞库：
    /// 密码接口不限速的话，攻击者能靠并发每秒试几千次。
    /// 5 次失败锁 10 分钟——就算前缀泄露被人拿到登录页，
    /// 一天最多也就试 700 来个密码，长随机密码根本试不动。
    pub fn lockout(&self) -> Option<u64> {
        let (n, last) = *self.fails.lock().unwrap();
        if n < MAX_FAILS {
            return None;
        }
        let e = last.elapsed().as_secs();
        (e < LOCK_SECS).then(|| LOCK_SECS - e)
    }

    pub fn on_fail(&self) {
        let mut f = self.fails.lock().unwrap();
        if f.0 >= MAX_FAILS && f.1.elapsed().as_secs() >= LOCK_SECS {
            *f = (0, Instant::now()); // 锁定期已过，重新计数
        }
        f.0 += 1;
        f.1 = Instant::now();
    }

    /// 登录成功后清掉失败计数
    pub fn reset_fails(&self) {
        *self.fails.lock().unwrap() = (0, Instant::now());
    }

    /// CPUUsageNSec 是开机以来的累计值，两次采样做差才是占用率。100% = 吃满一个核
    pub fn cpu(&self, unit: &str, cur: Option<u64>) -> Option<f64> {
        let cur = cur?;
        let now = Instant::now();
        let (prev, t) = self
            .cpu_prev
            .lock()
            .unwrap()
            .insert(unit.to_string(), (cur, now))?;
        let dt = now.duration_since(t).as_secs_f64();
        // 首次采样、间隔过短、或进程重启导致计数器归零，都算不出有意义的值
        (dt >= 0.5 && cur >= prev).then(|| (cur - prev) as f64 / 1e9 / dt * 100.0)
    }
}
