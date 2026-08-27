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
    /// 直接监听公网口，浏览器访问 http://你的IP:8080
    #[serde(default = "default_bind")]
    bind: String,
    /// 明文密码。配置文件记得 chmod 600
    password: String,
    /// 放项目的目录。里面每个子文件夹算一个项目，加项目不用改这里
    #[serde(default = "default_dirs")]
    dirs: Vec<String>,
    /// cargo 的绝对路径。留空自动探测——systemd 起进程时 PATH 里
    /// 通常没有 ~/.cargo/bin，所以不能直接写 "cargo"
    #[serde(default)]
    cargo: Option<String>,
}
fn default_bind() -> String {
    "0.0.0.0:8080".into()
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
    /// 上次是用哪个 bin、哪些参数起来的（面板重启后会丢，只影响界面回显）
    cur_bin: Option<String>,
    cur_args: Option<String>,
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
    /// 项目名 -> (bin, 参数)，「重启」要用它重新拼命令
    last: Mutex<HashMap<String, (String, String)>>,
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

const PROPS: &str = "LoadState,ActiveState,SubState,UnitFileState,MainPID,\
                     MemoryCurrent,CPUUsageNSec,ActiveEnterTimestampMonotonic";

#[derive(Default)]
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

async fn show(unit: &str) -> Raw {
    let args = ["show", "--no-pager", "--property", PROPS, "--", unit];
    let Ok((_, stdout, _)) = run("systemctl", &args).await else {
        return Raw::default();
    };
    let m: HashMap<&str, &str> = stdout.lines().filter_map(|l| l.split_once('=')).collect();
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

/// systemd 给的是 monotonic 时间戳，要配 /proc/uptime 才能换算成「运行了多久」
async fn boot_secs() -> f64 {
    tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
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

/// 扫配置里的目录，每个子文件夹算一个项目，返回 (项目名, 绝对路径)。
/// 每次请求都重新扫，所以新建文件夹后刷新网页就能看到，不用重启面板。
async fn discover(dirs: &[String]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(mut rd) = tokio::fs::read_dir(d).await else {
            continue; // 目录不存在就跳过，不影响其它目录
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            let name = e.file_name().to_string_lossy().into_owned();
            if is_dir && ok_name(&name) {
                out.push((name, e.path()));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// 每个项目对应一个 transient unit，名字加 panel- 前缀避免撞上系统里的服务
fn unit_of(project: &str) -> String {
    format!("panel-{project}.service")
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

async fn units(State(app): State<Arc<App>>) -> Json<Vec<Status>> {
    let boot = boot_secs().await;
    let found = discover(&app.cfg.dirs).await;
    let mut v = Vec::with_capacity(found.len());
    for (name, dir) in found {
        let unit = unit_of(&name);
        let r = show(&unit).await;
        let uptime = (r.active == "active")
            .then(|| r.active_since_us.map(|u| (boot - u as f64 / 1e6).max(0.0)))
            .flatten();
        let (cur_bin, cur_args) = match app.last.lock().unwrap().get(&name) {
            Some((b, a)) => (Some(b.clone()), Some(a.clone())),
            None => (None, None),
        };
        v.push(Status {
            key: name.clone(),
            bins: bins(&dir).await,
            name,
            cpu: app.cpu(&unit, r.cpu_nsec),
            unit,
            loaded: r.load == "loaded",
            enabled: r.enabled == "enabled",
            active: r.active,
            sub: r.sub,
            pid: r.pid,
            memory: r.memory,
            uptime,
            cur_bin,
            cur_args,
        });
    }
    Json(v)
}

/// 前端传来的名字一律重新扫目录核对，绝不直接拼进命令行
async fn resolve(app: &App, key: &str) -> Option<(String, PathBuf)> {
    if !ok_name(key) {
        return None;
    }
    discover(&app.cfg.dirs)
        .await
        .into_iter()
        .find(|(n, _)| n == key)
}

#[derive(Deserialize, Default)]
struct ActionReq {
    /// 要跑哪个 bin。stop 用不到；start/restart 不给就沿用上次的
    #[serde(default)]
    bin: Option<String>,
    /// 附加参数，空格分隔，原样传给程序（不过 shell，所以不用担心引号）
    #[serde(default)]
    args: Option<String>,
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
    // -- 之后的都是你程序自己的参数，cargo 不解释；不过 shell，所以引号空格都不用转义
    let extra: Vec<String> = args.split_whitespace().map(str::to_string).collect();
    if !extra.is_empty() {
        v.push("--".into());
        v.extend(extra);
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
    let unit = unit_of(project);
    // 上一次跑完/跑挂的同名 unit 还挂在那儿的话，systemd-run 会拒绝创建
    let _ = run("systemctl", &["reset-failed", "--", unit.as_str()]).await;

    let argv = systemd_run_argv(&app.cargo, &unit, &dir.display().to_string(), bin, args);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    match run("systemd-run", &refs).await {
        Ok((true, ..)) => {
            app.last
                .lock()
                .unwrap()
                .insert(project.to_string(), (bin.to_string(), args.to_string()));
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
    let unit = unit_of(&project);
    let req = body.map(|Json(b)| b).unwrap_or_default();

    // 没指定 bin 就沿用上次那个，重启按钮才能一键用
    let remembered = || app.last.lock().unwrap().get(&project).cloned();
    let pick = || match req.bin.clone() {
        Some(b) => Some((b, req.args.clone().unwrap_or_default())),
        None => remembered(),
    };

    let (ok, msg) = match act.as_str() {
        "stop" => match run("systemctl", &["stop", "--", unit.as_str()]).await {
            Ok((ok, out, err)) => (ok, if err.trim().is_empty() { out } else { err }),
            Err(e) => (false, e.to_string()),
        },
        "start" | "restart" => {
            let Some((bin, args)) = pick() else {
                return (StatusCode::BAD_REQUEST, "请先选一个要运行的程序").into_response();
            };
            if !ok_name(&bin) || !bins(&dir).await.contains(&bin) {
                return (StatusCode::BAD_REQUEST, "这个项目里没有这个程序").into_response();
            }
            if act == "restart" {
                let _ = run("systemctl", &["stop", "--", unit.as_str()]).await;
            }
            spawn(&app, &project, &dir, &bin, &args).await
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

#[derive(Deserialize)]
struct LogQuery {
    lines: Option<u32>,
}

async fn logs(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<LogQuery>,
) -> Response {
    let Some((project, _)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let unit = unit_of(&project);
    let n = q.lines.unwrap_or(300).clamp(1, 2000).to_string();
    let args = [
        "-u",
        unit.as_str(),
        "-n",
        &n,
        "--no-pager",
        "-o",
        "short-iso",
    ];
    match run("journalctl", &args).await {
        Ok((ok, out, err)) => {
            let body = if ok { out } else { format!("{out}{err}") };
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
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
    let dirs = cfg.dirs.clone();

    let app = Arc::new(App {
        cfg,
        cargo: cargo.clone(),
        sessions: Mutex::new(HashMap::new()),
        fails: Mutex::new((0, Instant::now())),
        cpu_prev: Mutex::new(HashMap::new()),
        last: Mutex::new(HashMap::new()),
    });

    // 除了首页和登录接口，其它一律要带有效 cookie
    let protected = Router::new()
        .route("/api/me", get(me))
        .route("/api/logout", post(logout))
        .route("/api/units", get(units))
        .route("/api/units/{key}/logs", get(logs))
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
    // 只是启动时打一眼方便对账，真正的列表是每次请求现扫的
    match discover(&dirs).await {
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
    fn 参数按空格拆成独立参数() {
        let v = systemd_run_argv(
            "/usr/bin/cargo",
            "panel-a.service",
            "/srv/a",
            "worker",
            "--port 8080  -v",
        );
        assert_eq!(&v[v.len() - 5..], &["worker", "--", "--port", "8080", "-v"]);
    }
}
