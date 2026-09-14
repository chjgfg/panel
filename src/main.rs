// 一个只做四件事的小面板：看状态、看日志、启动、停止/重启。
// 全部逻辑就这一个文件，网页是 static/index.html，编译时直接嵌进二进制。
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

type BoxErr = Box<dyn std::error::Error>;

const COOKIE: &str = "panel_session";
const SESSION_SECS: u64 = 7 * 86400;
const MAX_FAILS: u32 = 10;
const LOCK_SECS: u64 = 60;

#[derive(Deserialize)]
struct Config {
    /// 直接监听公网口，浏览器访问 http://你的IP（80 端口不用写端口号）
    #[serde(default = "default_bind")]
    bind: String,
    /// 明文密码。配置文件记得 chmod 600
    password: String,
    /// 放项目的目录。里面每个子文件夹算一个项目，加项目不用改这里
    #[serde(default = "default_dirs")]
    dirs: Vec<String>,
    /// 不想在页面上看到的项目。三种写法：
    ///   "scratch"      项目名，任何扫描目录下叫这个的都排掉
    ///   "test-*"       通配符，只支持 *
    ///   "/srv/x/old"   带 / 就按完整路径匹配，只排掉这一个
    #[serde(default)]
    exclude: Vec<String>,
    /// cargo 的绝对路径。留空自动探测——systemd 起进程时 PATH 里
    /// 通常没有 ~/.cargo/bin，所以不能直接写 "cargo"
    #[serde(default)]
    cargo: Option<String>,
}
fn default_bind() -> String {
    "0.0.0.0:80".into()
}
fn default_dirs() -> Vec<String> {
    vec!["/opt/apps".into()]
}

#[derive(Serialize)]
struct Status {
    key: String,
    name: String,
    unit: String,
    active: String,
    sub: String,
    loaded: bool,
    enabled: bool,
    pid: u64,
    memory: Option<u64>,
    cpu: Option<f64>,
    uptime: Option<f64>,
    /// 这个项目里所有能 cargo run --bin 的名字
    bins: Vec<String>,
    /// 每个可运行 bin 一条实例明细（名称/状态/pid/资源），前端展开项目行时逐行显示
    instances: Vec<BinInst>,
    /// 当前由面板起着（active）的 bin 名，前端靠它回显勾选
    running_bins: Vec<String>,
    /// 上次是用哪个 bin、哪些参数起来的（面板重启后会丢，只影响界面回显）
    cur_bin: Option<String>,
    cur_args: Option<String>,
    /// true = 这是你自己写的 xxx.service，面板只做启停，不选 bin
    external: bool,
    /// 面板外启动的进程 pid（终端里 cargo run 那种）。
    /// 有值就说明它在跑，但日志不在 journald 里，看不到。
    outside_pid: Option<u32>,
}

/// 一个项目下某个 bin 的实例明细。面板让每个 bin 独立成 unit，
/// 所以这里对着该 bin 的 unit 报它自己的状态、资源。
#[derive(Serialize)]
struct BinInst {
    bin: String,
    /// active / failed / inactive（该 bin 没跑）
    active: String,
    running: bool,
    pid: u64,
    memory: Option<u64>,
    cpu: Option<f64>,
    uptime: Option<f64>,
}

#[derive(Serialize, Clone)]
struct Disk {
    mount: String,
    used: u64,
    total: u64,
}

#[derive(Serialize)]
struct Host {
    /// 整机 CPU 占用百分比，首次请求拿不到（要两次采样做差）
    cpu: Option<f64>,
    cores: usize,
    load: [f64; 3],
    mem_used: u64,
    mem_total: u64,
    swap_used: u64,
    swap_total: u64,
    disks: Vec<Disk>,
    uptime: f64,
}

struct App {
    cfg: Config,
    /// cargo 的绝对路径，启动时定好
    cargo: String,
    /// token -> 过期时刻
    sessions: Mutex<HashMap<String, Instant>>,
    /// (连续失败次数, 最后一次失败时刻)
    fails: Mutex<(u32, Instant)>,
    /// unit -> (上次读到的 CPU 累计纳秒, 采样时刻)
    cpu_prev: Mutex<HashMap<String, (u64, Instant)>>,
    /// 项目名 -> 最近一次启动的那组 (bin, 参数)。「重启」要沿用它把整组重新拉起来，
    /// 所以要记一组而不是一个 bin。
    last: Mutex<HashMap<String, Vec<(String, String)>>>,
    /// 整机 CPU 的上次采样 (忙碌时间片, 总时间片)
    host_cpu: Mutex<Option<(u64, u64)>>,
    /// df 的结果缓存。磁盘占用变化很慢，没必要每 3 秒 fork 一个 df
    disks: Mutex<Option<(Instant, Vec<Disk>)>>,
}

impl App {
    fn valid_session(&self, tok: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, exp| *exp > now);
        s.contains_key(tok)
    }

    fn new_session(&self) -> Option<String> {
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
    fn lockout(&self) -> Option<u64> {
        let (n, last) = *self.fails.lock().unwrap();
        if n < MAX_FAILS {
            return None;
        }
        let e = last.elapsed().as_secs();
        (e < LOCK_SECS).then(|| LOCK_SECS - e)
    }

    fn on_fail(&self) {
        let mut f = self.fails.lock().unwrap();
        if f.0 >= MAX_FAILS && f.1.elapsed().as_secs() >= LOCK_SECS {
            *f = (0, Instant::now()); // 锁定期已过，重新计数
        }
        f.0 += 1;
        f.1 = Instant::now();
    }

    /// CPUUsageNSec 是开机以来的累计值，两次采样做差才是占用率。100% = 吃满一个核
    fn cpu(&self, unit: &str, cur: Option<u64>) -> Option<f64> {
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

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
}

// ---------- systemctl / journalctl ----------

const PROPS: &str = "Id,LoadState,ActiveState,SubState,UnitFileState,MainPID,\
                     MemoryCurrent,CPUUsageNSec,ActiveEnterTimestampMonotonic";

#[derive(Clone, Default)]
struct Raw {
    load: String,
    active: String,
    sub: String,
    enabled: String,
    pid: u64,
    memory: Option<u64>,
    cpu_nsec: Option<u64>,
    active_since_us: Option<u64>,
}

/// systemd 对「未设置」的数值属性会返回 u64::MAX 或 [not set]，都要当 None
fn num(v: &str) -> Option<u64> {
    match v.parse::<u64>() {
        Ok(n) if n != u64::MAX => Some(n),
        _ => None,
    }
}

/// 注意是直接 exec，不经过 shell，所以参数里有什么字符都不会被解释
async fn run(cmd: &str, args: &[&str]) -> std::io::Result<(bool, String, String)> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// 把 `systemctl show` 的一段输出解析成 Raw
fn parse_show(block: &str) -> Raw {
    let m: HashMap<&str, &str> = block.lines().filter_map(|l| l.split_once('=')).collect();
    let g = |k: &str| m.get(k).copied().unwrap_or("");
    Raw {
        load: g("LoadState").into(),
        active: g("ActiveState").into(),
        sub: g("SubState").into(),
        enabled: g("UnitFileState").into(),
        pid: num(g("MainPID")).unwrap_or(0),
        memory: num(g("MemoryCurrent")).filter(|&n| n > 0),
        cpu_nsec: num(g("CPUUsageNSec")),
        active_since_us: num(g("ActiveEnterTimestampMonotonic")).filter(|&n| n > 0),
    }
}

/// 一次 `systemctl show` 能查多个 unit，输出按空行分段，靠 Id= 认回是谁。
/// 这很重要：单核机器上每次刷新原来要 fork 十几个 systemctl，现在只要 1 个。
async fn show_many(units: &[String]) -> HashMap<String, Raw> {
    let mut out = HashMap::new();
    if units.is_empty() {
        return out;
    }
    let mut args = vec!["show", "--no-pager", "--property", PROPS, "--"];
    args.extend(units.iter().map(String::as_str));
    let Ok((_, stdout, _)) = run("systemctl", &args).await else {
        return out;
    };
    for block in stdout.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let raw = parse_show(block);
        let id = block
            .lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| *k == "Id")
            .map(|(_, v)| v.trim().to_string());
        if let Some(id) = id {
            out.insert(id, raw);
        }
    }
    out
}

/// systemd 给的是 monotonic 时间戳，要配 /proc/uptime 才能换算成「运行了多久」
async fn boot_secs() -> f64 {
    tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

// ---------- 整机资源 ----------

/// /proc/stat 第一行 `cpu  user nice system idle iowait ...` 是开机以来的累计
/// 时间片，两次采样做差才是占用率。idle 和 iowait 都算「没在干活」。
/// 返回 (忙碌, 总计)。
fn parse_cpu_line(s: &str) -> Option<(u64, u64)> {
    let rest = s.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 5 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let busy = total.checked_sub(v[3] + v[4])?;
    Some((busy, total))
}

/// /proc/meminfo 的值单位是 kB，这里统一换成字节
fn parse_meminfo(s: &str) -> HashMap<&str, u64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let n: u64 = v.split_whitespace().next()?.parse().ok()?;
            Some((k, n * 1024))
        })
        .collect()
}

fn parse_loadavg(s: &str) -> [f64; 3] {
    let mut out = [0.0; 3];
    for (i, x) in s.split_whitespace().take(3).enumerate() {
        out[i] = x.parse().unwrap_or(0.0);
    }
    out
}

/// 解析 `df -kP` 的输出。用 used+avail 当总量而不是第二列的 1K-blocks：
/// 后者含保留块，跟 df 自己算 Use% 的分母不一致，会显得对不上。
fn parse_df(s: &str) -> Vec<Disk> {
    let mut v: Vec<Disk> = Vec::new();
    for l in s.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        let (Ok(used), Ok(avail)) = (f[2].parse::<u64>(), f[3].parse::<u64>()) else {
            continue;
        };
        let mount = f[5].to_string();
        // 几个扫描目录常常在同一个分区上，同一挂载点只报一次
        if v.iter().any(|d| d.mount == mount) {
            continue;
        }
        v.push(Disk {
            mount,
            used: used * 1024,
            total: (used + avail) * 1024,
        });
    }
    v
}

async fn host_stats(app: &App) -> Host {
    let cpu = tokio::fs::read_to_string("/proc/stat")
        .await
        .ok()
        .as_deref()
        .and_then(parse_cpu_line)
        .and_then(|cur| {
            let prev = app.host_cpu.lock().unwrap().replace(cur);
            let (pb, pt) = prev?;
            let dt = cur.1.checked_sub(pt)?;
            let db = cur.0.checked_sub(pb)?;
            (dt > 0).then(|| db as f64 / dt as f64 * 100.0)
        });

    let mem = tokio::fs::read_to_string("/proc/meminfo")
        .await
        .unwrap_or_default();
    let mem = parse_meminfo(&mem);
    let g = |k: &str| mem.get(k).copied().unwrap_or(0);
    // 用 MemAvailable 而不是 MemFree：缓存那部分随时能让出来，不算「已用」
    let mem_total = g("MemTotal");
    let mem_used = mem_total.saturating_sub(g("MemAvailable"));
    let swap_total = g("SwapTotal");
    let swap_used = swap_total.saturating_sub(g("SwapFree"));

    let load = parse_loadavg(
        &tokio::fs::read_to_string("/proc/loadavg")
            .await
            .unwrap_or_default(),
    );

    // 只查关心的路径，省得把 tmpfs 和 snap 的 loop 设备全列出来。
    // 结果缓存 30 秒：磁盘占用变化很慢，不值得每次刷新都 fork 一个 df
    let cached = {
        let c = app.disks.lock().unwrap();
        c.as_ref()
            .filter(|(t, _)| t.elapsed() < Duration::from_secs(30))
            .map(|(_, d)| d.clone())
    };
    let disks = match cached {
        Some(d) => d,
        None => {
            let mut args = vec!["-kP", "--", "/"];
            args.extend(app.cfg.dirs.iter().map(String::as_str));
            let d = match run("df", &args).await {
                Ok((_, out, _)) => parse_df(&out),
                Err(_) => Vec::new(),
            };
            *app.disks.lock().unwrap() = Some((Instant::now(), d.clone()));
            d
        }
    };

    Host {
        cpu,
        cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
        load,
        mem_used,
        mem_total,
        swap_used,
        swap_total,
        disks,
        uptime: boot_secs().await,
    }
}

// ---------- 项目发现 ----------

/// 文件夹名会拼成 unit 名交给 systemctl，所以只放行安全字符。
/// 开头是 - 会被当成命令行选项，开头是 . 的是隐藏目录（.git 之类）。
fn ok_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && !n.starts_with(['-', '.'])
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
}

/// 只支持 * 的通配匹配，够用又不用引依赖。没有 * 就是全等。
fn glob_match(pat: &str, s: &str) -> bool {
    let segs: Vec<&str> = pat.split('*').collect();
    if segs.len() == 1 {
        return pat == s;
    }
    // 第一段必须顶在开头（pat 以 * 开头时这段是空串，恒成立）
    let Some(mut rest) = s.strip_prefix(segs[0]) else {
        return false;
    };
    let last = segs.len() - 1;
    for (i, seg) in segs.iter().enumerate().skip(1) {
        if i == last {
            // 最后一段必须落在末尾（pat 以 * 结尾时是空串，恒成立）
            return rest.ends_with(seg);
        }
        if seg.is_empty() {
            continue; // ** 跟 * 一个意思
        }
        let Some(at) = rest.find(seg) else {
            return false;
        };
        rest = &rest[at + seg.len()..];
    }
    true
}

/// 黑名单：带 / 的按完整路径比，不带的按项目名比
fn excluded(patterns: &[String], name: &str, dir: &std::path::Path) -> bool {
    let path = dir.to_string_lossy().replace('\\', "/");
    patterns.iter().any(|p| {
        if p.contains('/') {
            glob_match(p.trim_end_matches('/'), path.trim_end_matches('/'))
        } else {
            glob_match(p, name)
        }
    })
}

/// 面板自己的项目目录要排掉：它往往就在扫描目录里，
/// 但从面板里重启面板等于自杀，列出来只会误点。
/// 二进制在 <项目>/target/{debug,release}/panel，所以看 exe 是否在这个目录之下。
fn is_self(dir: &std::path::Path) -> bool {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return false;
    };
    dir.canonicalize().is_ok_and(|d| exe.starts_with(d))
}

/// 扫配置里的目录，每个子文件夹算一个项目，返回 (项目名, 绝对路径)。
/// 每次请求都重新扫，所以新建文件夹后刷新网页就能看到，不用重启面板。
async fn discover(dirs: &[String], exclude: &[String]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(mut rd) = tokio::fs::read_dir(d).await else {
            continue; // 目录不存在就跳过，不影响其它目录
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            let name = e.file_name().to_string_lossy().into_owned();
            if is_dir
                && ok_name(&name)
                && !is_self(&e.path())
                && !excluded(exclude, &name, &e.path())
            {
                out.push((name, e.path()));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// 扫一遍 /proc 拿到所有进程的 exe 路径。每次刷新只扫一次，
/// 再拿去跟各个项目目录比对，避免 N 个项目扫 N 遍 /proc。
/// 非 Linux（或 /proc 读不到）时返回空表，功能自动退化成「看不见外部进程」。
async fn proc_exes() -> Vec<(u32, PathBuf)> {
    let mut v = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir("/proc").await else {
        return v;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        // /proc 里除了 pid 还有 self、meminfo 之类，非数字的直接跳过
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(exe) = tokio::fs::read_link(format!("/proc/{pid}/exe")).await {
            v.push((pid, exe));
        }
    }
    v
}

/// 你在终端里 cargo run 起来的进程，systemd 不认识它，只能靠 exe 路径认：
/// 编出来的二进制一定在 <项目>/target/{debug,release}/ 下面。
/// 返回全部匹配的 pid —— cargo run 可能留下不止一个进程，只杀第一个不够。
fn find_outside(exes: &[(u32, PathBuf)], dir: &std::path::Path) -> Vec<u32> {
    let target = dir.join("target");
    exes.iter()
        .filter(|(_, exe)| exe.starts_with(&target))
        .map(|(pid, _)| *pid)
        .collect()
}

/// 进程还活着吗。僵尸进程要算死的：它已经放掉端口了，
/// 只是父进程还没回收，等它「消失」会白等 3 秒然后误报杀不掉。
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| alive_from_stat(&s))
}

/// /proc/<pid>/stat 格式是 `pid (comm) S ...`，comm 里可能有空格和括号
/// （进程名就叫 `foo (bar)` 也是合法的），所以状态字段要从最后一个 ) 往后取。
fn alive_from_stat(stat: &str) -> bool {
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
}

/// 先 TERM，等它们真的退出（最多 3 秒），赖着不走的补一发 KILL。
/// 必须等：端口是进程被回收之后才释放的，发完信号就返回会撞上 AddrInUse。
async fn stop_pids(pids: &[u32]) -> (bool, String) {
    if pids.is_empty() {
        return (true, String::new());
    }
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let signal = async |sig: &str| {
        let mut argv = vec![sig];
        argv.extend(list.iter().map(String::as_str));
        let _ = run("kill", &argv).await;
    };

    signal("-TERM").await;
    for _ in 0..30 {
        if !pids.iter().any(|p| alive(*p)) {
            return (true, String::new());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    signal("-KILL").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let left: Vec<String> = pids
        .iter()
        .filter(|p| alive(**p))
        .map(u32::to_string)
        .collect();
    if left.is_empty() {
        (true, String::new())
    } else {
        (false, format!("这些进程杀不掉：{}", left.join(", ")))
    }
}

/// 每个运行实例对应一个独立的 transient unit，名字加 panel- 前缀避免撞上系统里的服务。
/// 一个项目会计着多个 bin 同时跑，所以 unit 名要带上 bin —— systemd-run 不允许两个
/// active 的 unit 同名，不带 bin 的话第二个 bin 就起不来了。
fn unit_of(project: &str, bin: &str) -> String {
    format!("panel-{project}-{bin}.service")
}

/// 一个项目在面板里可能占着的全部候选 unit：
///   每个 bin 一个面板 unit（panel-{p}-{bin}.service），
///   加上你可能自己写过的 {p}.service。
/// 刷新和 stop 都要把「这一个项目」跟 unit 群对上号。
fn project_units(project: &str, bins: &[String]) -> Vec<String> {
    let mut v: Vec<String> = bins
        .iter()
        .map(|b| unit_of(project, b))
        .chain(std::iter::once(format!("{project}.service")))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// 面板自己起的、用于某个 bin 的 unit 名拼接出来的 bin（unit_of 的逆运算）。
/// 只用来把「哪些 bin 正在由面板跑着」翻译成前端可读的名字。
fn bin_of_panel_unit(project: &str, unit: &str) -> Option<String> {
    let prefix = format!("panel-{project}-");
    unit.strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(".service"))
        .map(str::to_string)
}

/// 一个项目在面板里可能同时跑着多个 bin，各自一个 panel-{p}-{bin} unit，
/// 也可能有你手写的 {p}.service。刷新时把「这一个项目」聚合成一个可见的行：
/// 挑一个 unit 当主 unit 报状态，并把「当前由面板起着、在跑的 bins」汇总出来
/// 给前端回显勾选。
struct Picked {
    /// 主 unit 名，展示和查日志用
    unit: String,
    /// 主 unit 是不是你自己写的 {p}.service（外部 unit，面板只做启停不选 bin）
    external: bool,
    raw: Raw,
    /// 当前由面板起着（active）的各 bin 名。前端靠它回显「这个项目现在跑着哪些 bin」
    running_bins: Vec<String>,
}

/// 从 show_many 的结果里把一个项目的全部 unit 收拢（只读，不改动 shown）。
/// 主 unit 挑选：面板的 bin unit 里谁在跑取谁；没在跑再退回你手写的 unit；
/// 都没有就按第一个面板 unit 报「未运行」。
fn pick_project(shown: &HashMap<String, Raw>, project: &str, bins: &[String]) -> Picked {
    let theirs = format!("{project}.service");

    let mut rows: Vec<(String, Raw)> = project_units(project, bins)
        .into_iter()
        .map(|u| {
            let r = shown.get(&u).cloned().unwrap_or_default();
            (u, r)
        })
        .collect();
    // 手写的 unit 总是最后一个，方便下面区分
    rows.sort_by_key(|(u, _)| if u.as_str() == theirs { 1 } else { 0 });

    // 正在由面板起着（active）的 bin
    let running_bins: Vec<String> = rows
        .iter()
        .filter(|(u, r)| u.as_str() != theirs && r.active == "active")
        .filter_map(|(u, _)| bin_of_panel_unit(project, u))
        .collect();

    // 主 unit：优先面板里正在跑的 bin unit，其次你手写的在跑的 unit，再退回第一个
    let primary = rows
        .iter()
        .find(|(u, r)| u.as_str() != theirs && r.active == "active")
        .or_else(|| rows.iter().find(|(u, r)| u.as_str() == theirs && r.active == "active"))
        .or_else(|| rows.first())
        .unwrap(); // bins 至少让 project_units 有一个面板 unit，不会空
    let external = primary.0.as_str() == theirs;

    Picked {
        unit: primary.0.clone(),
        external,
        raw: primary.1.clone(),
        running_bins,
    }
}

/// 单个项目用的版本：一次 systemctl show 查目标 bin 的所有候选名，一个进程搞定
async fn pick_project_checked(
    project: &str,
    bins: &[String],
) -> Picked {
    let cands = project_units(project, bins);
    let shown = show_many(&cands).await;
    pick_project(&shown, project, bins)
}

#[derive(Deserialize)]
struct CargoToml {
    package: Option<CargoPkg>,
}
#[derive(Deserialize)]
struct CargoPkg {
    name: String,
}

/// 列出项目里所有能 `cargo run --bin X` 的 X：
///   src/main.rs        -> Cargo.toml 里的包名（cargo 就是这么命名默认 bin 的）
///   src/bin/foo.rs     -> foo
///   src/bin/foo/main.rs -> foo
async fn bins(dir: &std::path::Path) -> Vec<String> {
    let mut v = Vec::new();

    if dir.join("src/main.rs").is_file()
        && let Ok(t) = tokio::fs::read_to_string(dir.join("Cargo.toml")).await
        && let Ok(ct) = toml::from_str::<CargoToml>(&t)
        && let Some(pkg) = ct.package
        && ok_name(&pkg.name)
    {
        v.push(pkg.name);
    }

    if let Ok(mut rd) = tokio::fs::read_dir(dir.join("src/bin")).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            let name = if p.extension().is_some_and(|x| x == "rs") {
                p.file_stem()
            } else if p.join("main.rs").is_file() {
                p.file_name()
            } else {
                None
            };
            if let Some(n) = name.map(|n| n.to_string_lossy().into_owned())
                && ok_name(&n)
            {
                v.push(n);
            }
        }
    }

    v.sort();
    v.dedup();
    v
}

/// systemd 起的进程 PATH 里没有 ~/.cargo/bin，所以要拿到 cargo 的绝对路径
fn find_cargo(explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit {
        return std::path::Path::new(p).is_file().then(|| p.to_string());
    }
    let mut cands = Vec::new();
    if let Ok(h) = std::env::var("HOME") {
        cands.push(format!("{h}/.cargo/bin/cargo"));
    }
    for p in [
        "/root/.cargo/bin/cargo",
        "/usr/local/cargo/bin/cargo",
        "/usr/local/bin/cargo",
        "/usr/bin/cargo",
    ] {
        cands.push(p.into());
    }
    cands
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
}

// ---------- 接口 ----------

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(State(app): State<Arc<App>>, Json(body): Json<LoginReq>) -> Response {
    if let Some(wait) = app.lockout() {
        let msg = format!("失败次数过多，请 {wait} 秒后再试");
        return (StatusCode::TOO_MANY_REQUESTS, msg).into_response();
    }
    if body.password != app.cfg.password {
        app.on_fail();
        return (StatusCode::UNAUTHORIZED, "密码错误").into_response();
    }
    *app.fails.lock().unwrap() = (0, Instant::now());

    let Some(tok) = app.new_session() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "无法生成会话").into_response();
    };
    let c = format!("{COOKIE}={tok}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECS}");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, c)]).into_response()
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&headers) {
        app.sessions.lock().unwrap().remove(t);
    }
    let c = format!("{COOKIE}=; Path=/; Max-Age=0");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, c)]).into_response()
}

/// 前端拿它判断「cookie 还有效吗」，能进来就说明有效
async fn me() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn host(State(app): State<Arc<App>>) -> Json<Host> {
    Json(host_stats(&app).await)
}

async fn units(State(app): State<Arc<App>>) -> Json<Vec<Status>> {
    let boot = boot_secs().await;
    let found = discover(&app.cfg.dirs, &app.cfg.exclude).await;

    // 先扫一遍每个项目的可选 bin（拼候选 unit 和 running_bins 都要用它）
    let mut found_bins: Vec<(String, Vec<String>)> = Vec::with_capacity(found.len());
    let mut cands: Vec<String> = Vec::new();
    for (n, dir) in &found {
        let b = bins(dir).await;
        cands.extend(project_units(n, &b));
        found_bins.push((n.clone(), b));
    }
    // 所有项目的全部候选 unit 一次问完 —— 原来是每个项目 fork 两次 systemctl
    let shown = show_many(&cands).await;
    let picked: Vec<Picked> = found
        .iter()
        .map(|(n, _)| {
            let b = found_bins
                .iter()
                .find(|(bn, _)| bn == n)
                .map(|(_, b)| b.clone())
                .unwrap_or_default();
            pick_project(&shown, n, &b)
        })
        .collect();

    // 只有存在「项目没有任何 unit 在跑」时才去扫 /proc —— 那一趟是几百次 readlink，
    // 单核机器上不该每 3 秒白跑一遍
    let any_down = picked.iter().any(|p| p.running_bins.is_empty() && p.raw.active != "active");
    let exes = if any_down { proc_exes().await } else { Vec::new() };

    let mut v = Vec::with_capacity(found.len());
    for ((name, dir), p) in found.into_iter().zip(picked) {
        // 关键是「有没有 unit 在跑」，不是「unit 存不存在」：
        // 上次启动失败会留下一个 failed 的 unit，它不该屏蔽掉裸进程检测
        let running = !p.running_bins.is_empty() || p.raw.active == "active";
        let outside = if running {
            Vec::new()
        } else {
            find_outside(&exes, &dir)
        };
        let outside_pid = outside.first().copied();
        let uptime = (p.raw.active == "active")
            .then(|| p.raw.active_since_us.map(|u| (boot - u as f64 / 1e6).max(0.0)))
            .flatten();
        let cur = app.last.lock().unwrap().get(&name).cloned();
        let cur_bin = cur.as_ref().and_then(|g| g.first()).map(|(b, _)| b.clone());
        let cur_args = cur.as_ref().and_then(|g| g.first()).map(|(_, a)| a.clone());
        let bins = found_bins
            .iter()
            .find(|(bn, _)| bn == &name)
            .map(|(_, b)| b.clone())
            .unwrap_or_default();
        // 每个 bin 一条实例：对着该 bin 的 unit 报状态和资源
        let instances: Vec<BinInst> = bins
            .iter()
            .map(|b| {
                let raw = shown.get(&unit_of(&name, b));
                let active = raw.map(|r| r.active.as_str()).unwrap_or("");
                let running = active == "active";
                let uptime = if running {
                    raw.and_then(|r| r.active_since_us)
                        .map(|u| (boot - u as f64 / 1e6).max(0.0))
                } else {
                    None
                };
                BinInst {
                    bin: b.clone(),
                    active: active.to_string(),
                    running,
                    pid: raw.map(|r| r.pid).unwrap_or(0),
                    memory: raw.and_then(|r| r.memory),
                    cpu: raw.and_then(|r| app.cpu(&unit_of(&name, b), r.cpu_nsec)),
                    uptime,
                }
            })
            .collect();

        // 项目一级行的资源，聚合该项目下所有「正在运行」的 bin：
        //   运行时长 = 取运行 bin 里最长那个；CPU = 运行 bin 百分比累加；内存 = 运行 bin 累加。
        // 一个 bin 都没在跑就显示 '-'（三种都置 None，前端统一画短横线）。
        // 外部 unit（external，你手写的 service）没有 bin 面板，项目行就是那一个 unit，
        // 所以保持主 unit 自己的数据，不套聚合。
        let (agg_cpu, agg_mem, agg_uptime) = if !p.external {
            let running: Vec<&BinInst> = instances.iter().filter(|i| i.running).collect();
            if running.is_empty() {
                (None, None, None)
            } else {
                let cpu = Some(running.iter().filter_map(|i| i.cpu).sum::<f64>());
                let memory = Some(running.iter().filter_map(|i| i.memory).sum::<u64>());
                // 运行中 bin 的 uptime 几乎都有值（active 就有 ActiveEnter 时刻），
                // 防一手全部采样失败的情况，取不到就别硬编一个
                let uptime = running.iter().filter_map(|i| i.uptime).reduce(f64::max);
                (cpu, memory, uptime)
            }
        } else {
            (app.cpu(&p.unit, p.raw.cpu_nsec), p.raw.memory, uptime)
        };

        v.push(Status {
            key: name.clone(),
            bins,
            instances,
            name,
            cpu: agg_cpu,
            unit: p.unit,
            external: p.external,
            loaded: p.raw.load == "loaded",
            enabled: p.raw.enabled == "enabled",
            active: p.raw.active,
            sub: p.raw.sub,
            pid: if p.raw.pid > 0 {
                p.raw.pid
            } else {
                outside_pid.unwrap_or(0) as u64
            },
            memory: agg_mem,
            uptime: agg_uptime,
            cur_bin,
            cur_args,
            running_bins: p.running_bins,
            outside_pid,
        });
    }
    Json(v)
}

/// 前端传来的名字一律重新扫目录核对，绝不直接拼进命令行
async fn resolve(app: &App, key: &str) -> Option<(String, PathBuf)> {
    if !ok_name(key) {
        return None;
    }
    discover(&app.cfg.dirs, &app.cfg.exclude)
        .await
        .into_iter()
        .find(|(n, _)| n == key)
}

#[derive(Deserialize, Default)]
struct ActionReq {
    /// 这次勾选要运行的 (bin, 该 bin 专属参数) 列表。stop 用不到；
    /// start/restart 为空就沿用上次那组，重启按钮才能一键用。
    #[serde(default)]
    bins: Vec<BinArg>,
}

#[derive(Deserialize)]
struct BinArg {
    bin: String,
    /// 该 bin 的启动参数，原样透传给程序，不做空格分割
    #[serde(default)]
    args: String,
}

/// 拼 systemd-run 的参数。抽成纯函数是为了能单测——真正跑起来只有 Linux 上能验。
fn systemd_run_argv(cargo: &str, unit: &str, dir: &str, bin: &str, args: &str) -> Vec<String> {
    let cargo_dir = std::path::Path::new(cargo)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut v = vec![
        format!("--unit={unit}"),
        format!("--working-directory={dir}"),
        // systemd 起的进程 PATH 很干净，cargo 自己还要找 rustc，得把它的目录带上
        format!("--setenv=PATH={cargo_dir}:/usr/local/bin:/usr/bin:/bin"),
        cargo.to_string(),
        "run".into(),
        "--bin".into(),
        bin.into(),
    ];
    // -- 之后的都是你程序自己的参数。不过 shell，所以引号空格都不用转义。
    // 需求：参数「原样透传」，不对空格做分割解析 —— 用户在这一行输入框里敲什么，
    // 就整串作为一个启动参数传给这个 bin。纯空白/空输入不加 `--`。
    if !args.trim().is_empty() {
        v.push("--".into());
        v.push(args.to_string());
    }
    v
}

/// 用 systemd-run 起一个 transient unit，等于临时造了个 systemd 服务。
/// 这样状态、运行时长、CPU、内存、日志全都沿用现成那套，
/// 面板不用自己管子进程和日志收集。
async fn spawn(
    app: &App,
    project: &str,
    dir: &std::path::Path,
    bin: &str,
    args: &str,
) -> (bool, String) {
    let unit = unit_of(project, bin);
    // 上一次跑完/跑挂的同名 unit 还挂在那儿的话，systemd-run 会拒绝创建
    let _ = run("systemctl", &["reset-failed", "--", unit.as_str()]).await;

    let argv = systemd_run_argv(&app.cargo, &unit, &dir.display().to_string(), bin, args);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    match run("systemd-run", &refs).await {
        Ok((true, ..)) => {
            let mut last = app.last.lock().unwrap();
            let group = last.entry(project.to_string()).or_default();
            // 同一批里重复养同一个 bin 就覆盖掉，不重复记
            if let Some(slot) = group.iter_mut().find(|(b, _)| b == bin) {
                *slot = (bin.to_string(), args.to_string());
            } else {
                group.push((bin.to_string(), args.to_string()));
            }
            (true, String::new())
        }
        Ok((false, out, err)) => {
            let msg = if err.trim().is_empty() { out } else { err };
            (false, msg.trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

async fn action(
    State(app): State<Arc<App>>,
    Path((key, act)): Path<(String, String)>,
    body: Option<Json<ActionReq>>,
) -> Response {
    let Some((project, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let this_bins = bins(&dir).await;
    // 聚合该项目当下所有 unit（每个 bin 一个面板 unit + 你手写的那个）
    let picked = pick_project_checked(&project, &this_bins).await;
    let external = picked.external;

    // unit 只要没在跑，就得去看是不是有个面板外的裸进程占着
    let outside = if picked.raw.active == "active" {
        Vec::new()
    } else {
        find_outside(&proc_exes().await, &dir)
    };

    // 现在就决定这组 (bin, 参数)：明确勾选了用它，没勾就沿用上次那组（重启按钮一键用）
    let chosen: Vec<(String, String)> = if !req.bins.is_empty() {
        let bad = req.bins.iter().find(|b| !ok_name(&b.bin) || !this_bins.contains(&b.bin));
        if bad.is_some() {
            return (StatusCode::BAD_REQUEST, "这个项目里没有这个程序").into_response();
        }
        req.bins
            .into_iter()
            .map(|b| (b.bin, b.args))
            .collect()
    } else {
        app.last.lock().unwrap().get(&project).cloned().unwrap_or_default()
    };

    // quiet=true 时「unit 根本没在跑」不算错（systemctl stop 一个没 loaded 的会报 not loaded，
    // 但停一个没跑的东西本来就该是空操作，不该弹红字）
    let sysctl_on = async |u: &str, act: &str, quiet: bool| -> (bool, String) {
        let (ok, msg) = match run("systemctl", &[act, "--", u]).await {
            Ok((ok, out, err)) => (ok, if err.trim().is_empty() { out } else { err }),
            Err(e) => (false, e.to_string()),
        };
        (ok || (quiet && !ok), msg)
    };

    let (ok, msg) = match act.as_str() {
        "stop" => {
            // 累加式下可能同时起着好几个 bin 的 unit，stop 把所有在跑的面板 unit、
            // 你手写的 unit、以及面板外的裸进程一律停掉
            let theirs = format!("{project}.service");
            let proj_units = project_units(&project, &this_bins);
            let shown = show_many(&proj_units).await;
            let mut failed = String::new();
            for u in &proj_units {
                let theirs_ok = *u == theirs;
                let active = shown.get(u).map_or(false, |r| r.active == "active");
                if active && (theirs_ok || u.starts_with(&format!("panel-{project}-"))) {
                    let (ok, msg) = sysctl_on(u, "stop", true).await;
                    if !ok && !msg.trim().is_empty() {
                        failed.push_str(&msg);
                    }
                }
            }
            if !outside.is_empty() {
                let (ok, msg) = stop_pids(&outside).await;
                if !ok {
                    failed.push_str(&msg);
                }
            }
            if failed.trim().is_empty() {
                (true, String::new())
            } else {
                (false, failed)
            }
        }
        // 你自己写的 unit，ExecStart 是你定的，面板不插手怎么起
        _ if external => match act.as_str() {
            "start" | "restart" => {
                let u = picked.unit.clone();
                sysctl_on(u.as_str(), act.as_str(), false).await
            }
            _ => return (StatusCode::BAD_REQUEST, "非法操作").into_response(),
        },
        "start" | "restart" => {
            if chosen.is_empty() {
                return (StatusCode::BAD_REQUEST, "请先选要运行的程序").into_response();
            }
            // 外面已经有一个在跑：必须等它真的退出再起，否则新进程会撞 AddrInUse
            if !outside.is_empty() {
                let (ok, msg) = stop_pids(&outside).await;
                if !ok {
                    return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
                }
            }
            // 累加式：对每个勾选的 bin 分别起；已经起着同 unit 的先停掉再起（保证用它当前参数）
            let mut errs = Vec::new();
            // 一次把所有候选 unit 的 ActiveState 问齐，循环里就不一个个 fork systemctl 了
            let active_units: Vec<String> = {
                let shown = show_many(&project_units(&project, &this_bins)).await;
                this_bins
                    .iter()
                    .map(|b| unit_of(&project, b))
                    .filter(|u| shown.get(u).map_or(false, |r| r.active == "active"))
                    .collect()
            };
            for (bin, args) in &chosen {
                let u = unit_of(&project, bin);
                // 若这个 bin 的 unit 还在 active，先停掉，否则 systemd-run 同名会拒绝
                if active_units.contains(&u) {
                    let _ = sysctl_on(&u, "stop", true).await;
                }
                let (ok, msg) = spawn(&app, &project, &dir, bin, args).await;
                if !ok {
                    let msg = msg.trim();
                    errs.push(if msg.is_empty() {
                        format!("「{bin}」起不来")
                    } else {
                        format!("「{bin}」：{msg}")
                    });
                }
            }
            if !errs.is_empty() {
                (false, errs.join("；"))
            } else {
                (true, String::new())
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "非法操作").into_response(),
    };

    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        let msg = msg.trim();
        let msg = if msg.is_empty() { "操作失败" } else { msg };
        (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
    }
}

/// 单个 bin 的启停：项目下每个 bin 独立成 unit，所以能单独停/重启某一个，
/// 不影响同项目其它在跑的 bin。前端 bin 条目的停止/重启按钮都走这里。
async fn bin_action(
    State(app): State<Arc<App>>,
    Path((key, bin, act)): Path<(String, String, String)>,
) -> Response {
    let Some((project, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    if !ok_name(&bin) || !bins(&dir).await.contains(&bin) {
        return (StatusCode::BAD_REQUEST, "这个项目里没有这个程序").into_response();
    }
    let unit = unit_of(&project, &bin);

    let sysctl = || async {
        match run("systemctl", &["stop", "--", unit.as_str()]).await {
            Ok((ok, out, err)) => (ok, if err.trim().is_empty() { out } else { err }),
            Err(e) => (false, e.to_string()),
        }
    };

    let (ok, msg) = match act.as_str() {
        // 单个 bin 的 unit 停掉；没在跑时 systemctl 报 not loaded，按空操作放行、不弹红字
        "stop" => {
            let (ok, msg) = sysctl().await;
            (ok || msg.contains("not loaded"), msg)
        }
        // 重启这一个 bin：停掉它的 unit 再按上次参数（或空参数）重新拉起
        "restart" => {
            let _ = sysctl().await;   // 已在跑的话先停，否则 systemd-run 同名会拒绝
            let args = app
                .last
                .lock()
                .unwrap()
                .get(&project)
                .cloned()
                .and_then(|g| g.into_iter().find(|(b, _)| b == &bin))
                .map(|(_, a)| a)
                .unwrap_or_default();
            spawn(&app, &project, &dir, &bin, &args).await
        }
        _ => {
            return (StatusCode::BAD_REQUEST, "非法操作").into_response();
        }
    };

    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        let msg = msg.trim();
        let msg = if msg.is_empty() { "操作失败" } else { msg };
        (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
    }
}

#[derive(Deserialize)]
struct LogQuery {
    lines: Option<u32>,
}

/// PID 1 自己关于 unit 说的话也归在这个 unit 名下，所以会混进项目日志里。
/// transient unit 一停，/run/systemd/transient 下的文件就被删了，之后 PID 1
/// 每次再去加载这个名字都会打一行 open 失败——它不代表任何故障，只会把日志
/// 刷满，所以丢掉。PID 1 别的话要留着：进程崩了、退出码是几，全靠它们看出来。
fn drop_noise(out: &str) -> String {
    let mut s = String::with_capacity(out.len());
    for line in out.lines() {
        if line.contains("/run/systemd/transient/") {
            continue;
        }
        s.push_str(line);
        s.push('\n');
    }
    s
}

#[derive(Serialize)]
struct BinLog {
    /// 来源 bin 名（面板起的 bin），或「外部」unit 时是这个项目名
    bin: String,
    /// the unit 的 journalctl 原文，short-iso，每行行首带时间戳
    log: String,
}

async fn logs(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<LogQuery>,
) -> Response {
    let Some((project, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let this_bins = bins(&dir).await;
    let picked = pick_project_checked(&project, &this_bins).await;
    let n = q.lines.unwrap_or(300).clamp(1, 2000).to_string();

    // 拉日志的目标：面板起的 bin 每个 unit 都拉（多 bin 各自独立日志）；
    // 手写的 unit（external）没有 bin，只有那一个 unit，标项目名。
    if picked.external {
        let unit = picked.unit;
        let args = ["-u", unit.as_str(), "-n", &n, "--no-pager", "-o", "short-iso"];
        let body = match run("journalctl", &args).await {
            Ok((ok, out, err)) => if ok {
                drop_noise(&out)
            } else {
                format!("{out}{err}")
            },
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        return Json(vec![BinLog { bin: format!("{project}（外部）"), log: body }]).into_response();
    }

    let mut out = Vec::with_capacity(this_bins.len());
    for b in &this_bins {
        let unit = unit_of(&project, b);
        let args = ["-u", unit.as_str(), "-n", &n, "--no-pager", "-o", "short-iso"];
        let body = match run("journalctl", &args).await {
            Ok((ok, out, _err)) if ok => drop_noise(&out),
            Ok((_, out, err)) => format!("{out}{err}"),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        out.push(BinLog { bin: b.clone(), log: body });
    }
    Json(out).into_response()
}

// ---------- 查看源码 ----------

/// 源码树里不放的名字：点开头的隐藏名（.git、.idea 之类）和 target（编译产物）。
fn tree_skip(name: &str) -> bool {
    name.starts_with('.') || name == "target"
}

/// 提供文件内容的大小上限（512KB）。超了只列名字不给内容，
/// 免得哪个大文件把整个 JSON 撑爆。
const MAX_SRC_FILE: u64 = 512 * 1024;

#[derive(Serialize)]
struct Node {
    name: String,
    /// 相对项目根的路径，用 / 连接
    path: String,
    dir: bool,
    /// 文件内容。None = 二进制（不是合法 UTF-8）或超过大小上限
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
}

/// 递归扫一个目录成一列 Node。目录在前、文件在后,各自按名字排。
/// 递归 async fn 必须装箱,否则 future 大小算不出来。
fn walk<'a>(dir: &'a std::path::Path, rel: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Node>> + Send + 'a>> {
    Box::pin(async move {
        let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if tree_skip(&name) {
            continue;
        }
        // 符号链接不跟：源码树里链接没什么可看的，不跟就不会有环
        let Ok(ft) = e.file_type().await else { continue };
        let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let node = if ft.is_dir() {
            let kids = walk(&e.path(), &path).await;
            Node { name, path, dir: true, content: None, children: kids }
        } else if ft.is_file() {
            // metadata 读不到就没法判大小，干脆不给内容
            let big = e.metadata().await.map_or(true, |m| m.len() > MAX_SRC_FILE);
            let content = if big {
                None
            } else {
                // 按 UTF-8 读不进来就是二进制，照样不给内容
                tokio::fs::read(&e.path()).await.ok().and_then(|b| String::from_utf8(b).ok())
            };
            Node { name, path, dir: false, content, children: Vec::new() }
        } else {
            continue; // socket、fifo 之类
        };
        out.push(node);
    }
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        out
    })
}

/// 项目目录树（内容一并带上，前端切文件不用再发请求）
async fn tree(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    let Some((name, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let children = walk(&dir, "").await;
    Json(Node { name, path: String::new(), dir: true, content: None, children }).into_response()
}

/// 在项目目录里 git pull 最新代码
async fn pull(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    let Some((_, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let d = dir.display().to_string();
    match run("git", &["-C", &d, "pull"]).await {
        Ok((true, out, _)) => (StatusCode::OK, out).into_response(),
        Ok((false, out, err)) => {
            let msg = if err.trim().is_empty() { out } else { err };
            (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn require_auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let ok = cookie_token(req.headers()).is_some_and(|t| app.valid_session(t));
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[tokio::main]
async fn main() {
    // 单独包一层：直接从 main 返回 Err 的话，输出是 Debug 格式
    // （Os { code: 2, ... } 这种），看不出到底哪个文件不见了
    if let Err(e) = start().await {
        eprintln!("面板启动失败：{e}");
        std::process::exit(1);
    }
}

/// 页面和接口都不许缓存。网页是 include_str! 编译进二进制的，
/// 面板升级后浏览器还拿着旧页面的话，症状会非常难查（后端新、前端旧）。
async fn no_cache(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// 找配置文件，按这个顺序：
///   1. 环境变量 PANEL_CONFIG
///   2. 当前目录下的 panel.toml       —— 在源码目录里 cargo run / ./target/debug/panel
///   3. 可执行文件旁边的 panel.toml   —— 部署成 /opt/panel/{panel, panel.toml}
///   4. /etc/panel.toml
///
/// 2 和 3 缺一不可：cargo run 时可执行文件在 target/debug/ 里，跟你放配置的
/// 项目根目录不是一个地方；而 systemd 启动服务时工作目录是 /，第 2 条又指不到。
fn config_path() -> Result<std::path::PathBuf, BoxErr> {
    if let Ok(p) = std::env::var("PANEL_CONFIG") {
        return Ok(p.into());
    }
    let mut tried = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        tried.push(cwd.join("panel.toml"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        tried.push(dir.join("panel.toml"));
    }
    tried.push("/etc/panel.toml".into());
    tried.dedup(); // 直接在部署目录里跑的时候，前两条是同一个路径

    if let Some(found) = tried.iter().find(|p| p.is_file()) {
        return Ok(found.clone());
    }
    let list: Vec<String> = tried.iter().map(|p| format!("  {}", p.display())).collect();
    Err(format!(
        "找不到配置文件，这几个位置都看过了：\n{}\n\
         照 panel.toml.example 改一份，放到上面任意一个位置；\
         或者用 PANEL_CONFIG=/你的/路径 指定",
        list.join("\n")
    )
    .into())
}

async fn start() -> Result<(), BoxErr> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读不到配置文件 {}（{e}）", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).map_err(|e| format!("配置文件 {} 有问题：{e}", path.display()))?;

    if cfg.password.chars().count() < 12 {
        return Err(
            "password 至少 12 位：面板裸挂公网，弱密码几小时内就会被试出来。\
                    用 `openssl rand -base64 24` 生成一个"
                .into(),
        );
    }
    if cfg.dirs.is_empty() {
        return Err("dirs 不能为空：至少给一个放项目的目录".into());
    }
    for d in &cfg.dirs {
        // 目录不存在不算致命错误：你可能打算稍后再建
        if !std::path::Path::new(d).is_dir() {
            eprintln!("提示：目录 {d} 目前不存在，扫描时会跳过");
        }
    }
    let cargo = match find_cargo(cfg.cargo.as_deref()) {
        Some(c) => c,
        None => {
            return Err("找不到 cargo。在 panel.toml 里加一行指明路径，\
                        比如 cargo = \"/root/.cargo/bin/cargo\"（用 which cargo 查）"
                .into());
        }
    };
    let bind = cfg.bind.clone();
    let exclude = cfg.exclude.clone();
    let dirs = cfg.dirs.clone();

    let app = Arc::new(App {
        cfg,
        cargo: cargo.clone(),
        sessions: Mutex::new(HashMap::new()),
        fails: Mutex::new((0, Instant::now())),
        cpu_prev: Mutex::new(HashMap::new()),
        last: Mutex::new(HashMap::new()),
        host_cpu: Mutex::new(None),
        disks: Mutex::new(None),
    });

    // 除了首页和登录接口，其它一律要带有效 cookie
    let protected = Router::new()
        .route("/api/me", get(me))
        .route("/api/logout", post(logout))
        .route("/api/units", get(units))
        .route("/api/host", get(host))
        .route("/api/units/{key}/logs", get(logs))
        .route("/api/units/{key}/tree", get(tree))
        .route("/api/units/{key}/pull", get(pull))
        .route("/api/units/{key}/bins/{bin}/{action}", post(bin_action))
        .route("/api/units/{key}/{action}", post(action))
        .layer(middleware::from_fn_with_state(app.clone(), require_auth));

    let router = Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .merge(protected)
        .layer(middleware::from_fn(no_cache))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("监听 {bind} 失败：{e}"))?;
    println!("配置: {}", path.display());
    println!("cargo: {cargo}");
    println!("扫描目录: {}", dirs.join(", "));
    if !exclude.is_empty() {
        println!("黑名单: {}", exclude.join(", "));
    }
    // 只是启动时打一眼方便对账，真正的列表是每次请求现扫的
    match discover(&dirs, &exclude).await {
        v if v.is_empty() => println!("当前扫到的项目: (无)"),
        v => {
            for (name, dir) in v {
                let b = bins(&dir).await;
                let b = if b.is_empty() {
                    "没找到可运行的 bin".to_string()
                } else {
                    b.join(" / ")
                };
                println!("  {name}: {b}");
            }
        }
    }
    println!("面板已启动: http://{}", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 文件夹名只放行安全字符() {
        assert!(ok_name("blog"));
        assert!(ok_name("my-api_2.0"));
        assert!(ok_name("tpl@inst"));
        assert!(!ok_name(""));
        assert!(!ok_name(".git")); // 隐藏目录
        assert!(!ok_name("-rf")); // 会被当成命令行选项
        assert!(!ok_name("a b")); // 空格
        assert!(!ok_name("../etc")); // 路径穿越
        assert!(!ok_name("naïve")); // 非 ASCII
        assert!(!ok_name(&"x".repeat(101)));
    }

    #[test]
    fn 不带参数时不加双横线() {
        let v = systemd_run_argv(
            "/root/.cargo/bin/cargo",
            "panel-blog.service",
            "/opt/apps/blog",
            "importer",
            "   ",
        );
        assert_eq!(
            v,
            vec![
                "--unit=panel-blog.service",
                "--working-directory=/opt/apps/blog",
                "--setenv=PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
                "/root/.cargo/bin/cargo",
                "run",
                "--bin",
                "importer",
            ]
        );
    }

    #[test]
    fn 僵尸进程算死的() {
        assert!(alive_from_stat("69968 (xau) R 1 69968 69968 0 -1 4194560"));
        assert!(alive_from_stat("69968 (xau) S 1 69968"));
        assert!(!alive_from_stat("69968 (xau) Z 1 69968"));
        // 进程名里带空格和括号是合法的，状态字段必须从最后一个 ) 往后取
        assert!(alive_from_stat("42 (my (weird) app) R 1 42"));
        assert!(!alive_from_stat("42 (my (weird) app) Z 1 42"));
        // 名字里有 z 不该被当成僵尸
        assert!(alive_from_stat("42 (zombie-hunter) S 1 42"));
        assert!(!alive_from_stat("")); // 读不到就当死了
    }

    #[test]
    fn 只认target目录下的进程() {
        let exes = vec![
            (
                1u32,
                PathBuf::from("/root/rust_project/xau/target/debug/xau"),
            ),
            (2, PathBuf::from("/root/.cargo/bin/cargo")),
            (
                3,
                PathBuf::from("/root/rust_project/xau/target/release/shell"),
            ),
            (
                4,
                PathBuf::from("/root/rust_project/other/target/debug/other"),
            ),
            (5, PathBuf::from("/usr/bin/sshd")),
        ];
        let dir = PathBuf::from("/root/rust_project/xau");
        assert_eq!(find_outside(&exes, &dir), vec![1, 3]);
        assert!(find_outside(&exes, &PathBuf::from("/root/rust_project/nope")).is_empty());
    }

    #[test]
    fn 通配符只认星号() {
        assert!(glob_match("panel", "panel"));
        assert!(!glob_match("panel", "panel2"));
        assert!(glob_match("test-*", "test-a"));
        assert!(glob_match("test-*", "test-")); // * 可以匹配空
        assert!(!glob_match("test-*", "tes"));
        assert!(glob_match("*-old", "proj-old"));
        assert!(!glob_match("*-old", "proj-new"));
        assert!(glob_match("*tmp*", "my-tmp-thing"));
        assert!(glob_match("a*b", "ab")); // 中间可以是空
        assert!(!glob_match("a*b", "a"));
        assert!(glob_match("*", "随便什么"));
        assert!(glob_match("/srv/*/old", "/srv/x/old"));
        assert!(!glob_match("/srv/*/old", "/srv/x/new"));
    }

    #[test]
    fn 黑名单按名字或路径匹配() {
        let dir = PathBuf::from("/root/rust_project/panel");
        // 不带 / 的按项目名比
        assert!(excluded(&["panel".into()], "panel", &dir));
        assert!(!excluded(&["panel".into()], "xau", &dir));
        assert!(excluded(&["pa*".into()], "panel", &dir));
        // 带 / 的按完整路径比，同名但不同路径的不受影响
        assert!(excluded(
            &["/root/rust_project/panel".into()],
            "panel",
            &dir
        ));
        assert!(!excluded(&["/srv/apps/panel".into()], "panel", &dir));
        assert!(excluded(&["/root/rust_project/*".into()], "panel", &dir));
        // 末尾多个斜杠不该影响判断
        assert!(excluded(
            &["/root/rust_project/panel/".into()],
            "panel",
            &dir
        ));
        assert!(!excluded(&[], "panel", &dir));
    }

    #[test]
    fn 整机cpu两次采样做差() {
        // idle(3) 和 iowait(4) 算没干活，其余都算忙
        let a = "cpu  100 0 100 800 0 0 0 0 0 0\nintr 1\n";
        let b = "cpu  200 0 100 1000 0 0 0 0 0 0\nintr 1\n";
        assert_eq!(parse_cpu_line(a), Some((200, 1000)));
        assert_eq!(parse_cpu_line(b), Some((300, 1300)));
        // 忙碌增量 100 / 总增量 300 ≈ 33.3%
        let (b0, t0) = parse_cpu_line(a).unwrap();
        let (b1, t1) = parse_cpu_line(b).unwrap();
        let pct = (b1 - b0) as f64 / (t1 - t0) as f64 * 100.0;
        assert!((pct - 33.333).abs() < 0.01, "{pct}");
        // 字段不够、或者压根不是 cpu 行，都要给 None 而不是 panic
        assert_eq!(parse_cpu_line("cpu  1 2 3\n"), None);
        assert_eq!(parse_cpu_line("cpu0 1 2 3 4 5\n"), None);
        assert_eq!(parse_cpu_line(""), None);
    }

    #[test]
    fn 内存按可用量算已用() {
        let s = "MemTotal:        4030464 kB\n\
                 MemFree:          123456 kB\n\
                 MemAvailable:    3000000 kB\n\
                 SwapTotal:             0 kB\n\
                 SwapFree:              0 kB\n";
        let m = parse_meminfo(s);
        assert_eq!(m.get("MemTotal"), Some(&(4030464 * 1024)));
        // 已用 = Total - Available，不是 Total - Free（缓存随时能让出来）
        assert_eq!(m["MemTotal"] - m["MemAvailable"], 1030464 * 1024);
        assert_eq!(m.get("SwapTotal"), Some(&0));
        assert_eq!(parse_meminfo("垃圾行\n").len(), 0);
    }

    #[test]
    fn 负载取前三个数() {
        assert_eq!(
            parse_loadavg("0.42 0.30 0.25 1/234 5678\n"),
            [0.42, 0.30, 0.25]
        );
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn df输出解析且挂载点去重() {
        let s = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                 /dev/sda1         50432764 18000000  29000000      39% /\n\
                 /dev/sda1         50432764 18000000  29000000      39% /\n\
                 tmpfs               403044        0    403044       0% /dev/shm\n\
                 坏行\n";
        let d = parse_df(s);
        assert_eq!(d.len(), 2, "同一挂载点只能出现一次");
        assert_eq!(d[0].mount, "/");
        assert_eq!(d[0].used, 18_000_000 * 1024);
        // 总量用 used+avail，跟 df 自己算 Use% 的分母一致
        assert_eq!(d[0].total, (18_000_000 + 29_000_000) * 1024);
        assert_eq!(d[1].mount, "/dev/shm");
        assert!(parse_df("只有表头\n").is_empty());
    }

    #[test]
    fn 一次show多个unit按id分段() {
        // systemctl show 多个 unit 时，每段之间是一个空行
        let out = "Id=panel-xau.service\nLoadState=not-found\nActiveState=inactive\n\
                   SubState=dead\nUnitFileState=\nMainPID=0\nMemoryCurrent=[not set]\n\
                   CPUUsageNSec=[not set]\nActiveEnterTimestampMonotonic=0\n\
                   \n\
                   Id=xau.service\nLoadState=loaded\nActiveState=active\nSubState=running\n\
                   UnitFileState=enabled\nMainPID=4242\nMemoryCurrent=52428800\n\
                   CPUUsageNSec=1500000000\nActiveEnterTimestampMonotonic=9000000\n";
        let mut m: HashMap<String, Raw> = HashMap::new();
        for block in out.split("\n\n") {
            if block.trim().is_empty() {
                continue;
            }
            let id = block
                .lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| *k == "Id")
                .map(|(_, v)| v.trim().to_string())
                .unwrap();
            m.insert(id, parse_show(block));
        }
        assert_eq!(m.len(), 2);
        assert_eq!(m["panel-xau.service"].load, "not-found");
        assert_eq!(m["xau.service"].active, "active");
        assert_eq!(m["xau.service"].pid, 4242);
        assert_eq!(m["xau.service"].memory, Some(52428800));
        // [not set] 要当 None，不能 panic
        assert_eq!(m["panel-xau.service"].memory, None);
        assert_eq!(m["panel-xau.service"].active_since_us, None);

        // 手写的 xau.service 在跑 —— 但面板自己的 bin unit 在跑时优先作为主子 unit，
        // 手写的只算备选；这里只有手写的在跑，所以选它并标成 external
        let m2 = m.clone();
        let picked = pick_project(&m2, "xau", &["web".to_string()]);
        assert_eq!(picked.unit, "xau.service");
        assert!(picked.external);
        assert_eq!(picked.raw.pid, 4242);
        assert!(picked.running_bins.is_empty());
    }

    #[test]
    fn 多bin同时起着各自归进running_bins且以面板unit为主() {
        let mk = |load: &str, active: &str| Raw {
            load: load.into(),
            active: active.into(),
            enabled: "enabled".into(),
            pid: 1000,
            ..Raw::default()
        };
        // 项目 xau 两个 bin 同时起着，你手写的 xau.service 也在跑
        let m = HashMap::from([
            ("panel-xau-web.service".to_string(), mk("loaded", "active")),
            ("panel-xau-api.service".to_string(), mk("loaded", "active")),
            ("xau.service".to_string(), mk("loaded", "active")),
        ]);
        let bins = vec!["web".to_string(), "api".to_string()];
        let p = pick_project(&m, "xau", &bins);
        // 主 unit 优先面板里在跑的 bin unit（具体是哪个 bin 无关紧要）
        assert!(!p.external);
        assert!(p.unit.starts_with("panel-xau-"));
        // 两个在跑的 bin 都要汇总给前端回显
        assert_eq!(p.running_bins.len(), 2);
        assert!(p.running_bins.contains(&"web".to_string()));
        assert!(p.running_bins.contains(&"api".to_string()));

        // 一个都没跑时：按某个面板 unit 报未运行，主 unit 算面板的、不算 external
        let empty: HashMap<String, Raw> = HashMap::new();
        let p2 = pick_project(&empty, "xau", &bins);
        assert!(!p2.external);
        assert!(p2.unit.starts_with("panel-xau-"));
        assert_eq!(p2.raw.load, "");
        assert!(p2.running_bins.is_empty());
    }

    #[test]
    fn 参数原样透传不按空格拆分() {
        let v = systemd_run_argv(
            "/usr/bin/cargo",
            "panel-a.service",
            "/srv/a",
            "worker",
            "--port 8080  -v",
        );
        // 参数作为一个整体透传给程序，不把空格当分隔符拆成多个参数
        assert_eq!(&v[v.len() - 3..], &["worker", "--", "--port 8080  -v"]);
    }

    #[test]
    fn 源码树过滤隐藏目录和target() {
        assert!(tree_skip(".git"));
        assert!(tree_skip(".idea"));
        assert!(tree_skip("target"));
        // 普通名字都放行，包括点在中间的
        assert!(!tree_skip("src"));
        assert!(!tree_skip("panel.toml"));
        assert!(!tree_skip("my.dir"));
    }

    #[test]
    fn transient文件没了的噪声行不进日志() {
        let out = "2026-08-29T09:34:09+00:00 h systemd[1]: panel-xau.service: \
                   Failed to open /run/systemd/transient/panel-xau.service: \
                   No such file or directory\n\
                   2026-08-29T09:37:23+00:00 h systemd[1]: Started panel-xau.service - cargo run.\n\
                   2026-08-29T09:37:24+00:00 h xau[123]: listening on 8080\n\
                   2026-08-29T09:38:00+00:00 h systemd[1]: panel-xau.service: \
                   Main process exited, code=exited, status=101/n/a\n";
        let got = drop_noise(out);
        assert!(!got.contains("Failed to open"));
        // 启停和崩溃这几行是有用的，不能跟着一起丢
        assert!(got.contains("Started panel-xau.service"));
        assert!(got.contains("listening on 8080"));
        assert!(got.contains("status=101"));
        assert_eq!(got.lines().count(), 3);
    }
}
