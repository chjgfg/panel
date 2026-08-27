// 一个只做四件事的小面板：看状态、看日志、启动、停止/重启。
// 全部逻辑就这一个文件，网页是 static/index.html，编译时直接嵌进二进制。
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
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
    projects: Vec<Project>,
}
fn default_bind() -> String {
    "0.0.0.0:8080".into()
}

#[derive(Deserialize)]
struct Project {
    key: String,
    name: String,
    unit: String,
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
}

struct App {
    cfg: Config,
    /// token -> 过期时刻
    sessions: Mutex<HashMap<String, Instant>>,
    /// (连续失败次数, 最后一次失败时刻)
    fails: Mutex<(u32, Instant)>,
    /// unit -> (上次读到的 CPU 累计纳秒, 采样时刻)
    cpu_prev: Mutex<HashMap<String, (u64, Instant)>>,
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
    let mut v = Vec::with_capacity(app.cfg.projects.len());
    for p in &app.cfg.projects {
        let r = show(&p.unit).await;
        let uptime = (r.active == "active")
            .then(|| r.active_since_us.map(|u| (boot - u as f64 / 1e6).max(0.0)))
            .flatten();
        v.push(Status {
            key: p.key.clone(),
            name: p.name.clone(),
            unit: p.unit.clone(),
            cpu: app.cpu(&p.unit, r.cpu_nsec),
            loaded: r.load == "loaded",
            enabled: r.enabled == "enabled",
            active: r.active,
            sub: r.sub,
            pid: r.pid,
            memory: r.memory,
            uptime,
        });
    }
    Json(v)
}

async fn action(State(app): State<Arc<App>>, Path((key, act)): Path<(String, String)>) -> Response {
    if !matches!(act.as_str(), "start" | "stop" | "restart") {
        return (StatusCode::BAD_REQUEST, "非法操作").into_response();
    }
    // 前端传来的 key 只用来查配置表，unit 名永远取自配置文件
    let Some(p) = app.cfg.projects.iter().find(|p| p.key == key) else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    match run("systemctl", &[act.as_str(), p.unit.as_str()]).await {
        Ok((true, ..)) => StatusCode::NO_CONTENT.into_response(),
        Ok((false, out, err)) => {
            let msg = if err.trim().is_empty() { out } else { err };
            (StatusCode::INTERNAL_SERVER_ERROR, msg.trim().to_string()).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    let Some(p) = app.cfg.projects.iter().find(|p| p.key == key) else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let n = q.lines.unwrap_or(300).clamp(1, 2000).to_string();
    let args = [
        "-u",
        p.unit.as_str(),
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

async fn start() -> Result<(), BoxErr> {
    let path = std::env::var("PANEL_CONFIG").unwrap_or_else(|_| "/etc/panel.toml".into());
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "读不到配置文件 {path}（{e}）\n\
             照 panel.toml.example 建一个：cp panel.toml.example /etc/panel.toml && chmod 600 /etc/panel.toml\n\
             想放别的位置就设环境变量 PANEL_CONFIG=/你的/路径"
        )
    })?;
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("配置文件 {path} 有问题：{e}"))?;

    if cfg.password.chars().count() < 12 {
        return Err(
            "password 至少 12 位：面板裸挂公网，弱密码几小时内就会被试出来。\
                    用 `openssl rand -base64 24` 生成一个"
                .into(),
        );
    }
    let mut keys = std::collections::HashSet::new();
    for p in &cfg.projects {
        if !keys.insert(&p.key) {
            return Err(format!("重复的 key: {}", p.key).into());
        }
    }
    let bind = cfg.bind.clone();

    let app = Arc::new(App {
        cfg,
        sessions: Mutex::new(HashMap::new()),
        fails: Mutex::new((0, Instant::now())),
        cpu_prev: Mutex::new(HashMap::new()),
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
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("监听 {bind} 失败：{e}"))?;
    println!("面板已启动: http://{}", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}
