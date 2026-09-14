// 一个只做四件事的小面板：看状态、看日志、启动、停止/重启。
// 网页是 static/index.html，编译时直接嵌进二进制。
//
// 模块划分：
//   config   配置文件结构与查找
//   state    全局共享状态（会话表、采样缓存）
//   auth     登录/登出/会话校验
//   hostinfo 整机资源（/proc 与 df）
//   discover 项目发现（扫目录、黑名单）
//   systemd  systemctl/journalctl 交互
// 这里只剩 web 层：路由、handler、源码树和进程管理。
mod auth;
mod config;
mod discover;
mod hostinfo;
mod state;
mod systemd;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::{BoxErr, Config, config_path};
use crate::discover::{discover, ok_name};
use crate::state::App;
use crate::systemd::{
    Picked, boot_secs, pick_project, pick_project_checked, project_units, run, show_many, unit_of,
};

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

// ---------- 面板外进程 ----------

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

// ---------- 接口 ----------

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}

// 语法高亮用 highlight.js，和页面一样编译时嵌进二进制，部署不依赖外网。
// 文件名带版本号，升级换文件名即可让缓存失效，可以放心设长缓存
// （no_cache 中间件里对 /vendor/ 单独放行）。
// token 配色不用官方主题 CSS：它把 code.hljs 设成 display:block 会打断
// 行内布局，且十几类 token 挤一两个颜色分不出来——配色面板自己写。
async fn hljs_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_str!("../static/vendor/highlight.11.12.0.min.js"),
    )
}

async fn host(State(app): State<Arc<App>>) -> Json<hostinfo::Host> {
    Json(hostinfo::host_stats(&app).await)
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

/// 源码树里不放的名字：点开头的隐藏项（.git、.idea 之类）、编译产物和依赖目录。
/// 例外白名单：.env.example、.gitignore 这类常要看的点文件放行。
/// node_modules/vendor 这类目录一个就好几万文件，进来树就废了。
fn tree_skip(name: &str) -> bool {
    const DOTFILES_KEEP: &[&str] = &[
        ".env.example", ".env.local.example", ".env.sample",
        ".gitignore", ".gitattributes", ".dockerignore",
        ".editorconfig", ".npmrc", ".nvmrc", ".rustfmt.toml", ".rust-toolchain",
    ];
    const DIRS_SKIP: &[&str] = &[
        "target", "node_modules", "vendor", "dist", "build",
        "out", "__pycache__", "venv", ".venv",
    ];
    (name.starts_with('.') && !DOTFILES_KEEP.contains(&name)) || DIRS_SKIP.contains(&name)
}

/// 提供文件内容的大小上限（512KB）。超了只列名字不给内容，
/// 免得哪个大文件把响应撑爆。
const MAX_SRC_FILE: u64 = 512 * 1024;

#[derive(Serialize)]
struct Node {
    name: String,
    /// 相对项目根的路径，用 / 连接
    path: String,
    dir: bool,
    /// 文件超过 512KB：树里就标出来，前端点都不用点
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    big: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
}

/// 递归扫一个目录成一列 Node。只出结构不读内容——文件内容走 /file 懒加载，
/// 否则项目一大，扫树就得把几百个文件挨个读一遍。
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
            Node { name, path, dir: true, big: false, children: kids }
        } else if ft.is_file() {
            // metadata 读不到就当大文件处理，免得点了报错
            let big = e.metadata().await.map_or(true, |m| m.len() > MAX_SRC_FILE);
            Node { name, path, dir: false, big, children: Vec::new() }
        } else {
            continue; // socket、fifo 之类
        };
        out.push(node);
    }
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        out
    })
}

/// 项目目录树（只给结构，内容点开文件时另走 /file，见下）
async fn tree(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    let Some((name, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let children = walk(&dir, "").await;
    Json(Node { name, path: String::new(), dir: true, big: false, children }).into_response()
}

/// 懒加载取文件时的路径检查：必须是相对路径，不带 .. 穿越到项目外，
/// 且路径上每段都是树里会显示的名字（node_modules 之类就算拼 URL 也读不到）
fn file_path_ok(path: &str) -> bool {
    !path.starts_with('/')
        && path.split('/').all(|seg| !seg.is_empty() && seg != ".." && !tree_skip(seg))
}

#[derive(Serialize)]
struct FileBody {
    /// None = 二进制（不是合法 UTF-8）。大小上限在树里已标 big，这里再防一道
    content: Option<String>,
}

/// 单个文件内容。树接口只给结构，这里点哪个文件读哪个
async fn file(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some((_, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let Some(path) = q.get("path") else {
        return (StatusCode::BAD_REQUEST, "缺 path 参数").into_response();
    };
    if !file_path_ok(path) {
        return (StatusCode::BAD_REQUEST, "非法路径").into_response();
    }
    let full = dir.join(path);
    let Ok(meta) = tokio::fs::metadata(&full).await else {
        return (StatusCode::NOT_FOUND, "文件不存在").into_response();
    };
    if !meta.is_file() {
        return (StatusCode::BAD_REQUEST, "不是文件").into_response();
    }
    let content = if meta.len() > MAX_SRC_FILE {
        None
    } else {
        // 按 UTF-8 读不进来就是二进制，照样不给内容
        tokio::fs::read(&full).await.ok().and_then(|b| String::from_utf8(b).ok())
    };
    Json(FileBody { content }).into_response()
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
/// vendor 下的静态资源除外：文件名带 hash，内容永远不会变。
async fn no_cache(req: Request, next: Next) -> Response {
    let vendor = req.uri().path().starts_with("/vendor/");
    let mut res = next.run(req).await;
    if !vendor {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
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

    let app = App::new(cfg, cargo.clone());

    // 除了首页和登录接口，其它一律要带有效 cookie
    let protected = Router::new()
        .route("/api/me", get(auth::me))
        .route("/api/logout", post(auth::logout))
        .route("/api/units", get(units))
        .route("/api/host", get(host))
        .route("/api/units/{key}/logs", get(logs))
        .route("/api/units/{key}/tree", get(tree))
        .route("/api/units/{key}/file", get(file))
        .route("/api/units/{key}/pull", get(pull))
        .route("/api/units/{key}/bins/{bin}/{action}", post(bin_action))
        .route("/api/units/{key}/{action}", post(action))
        .layer(middleware::from_fn_with_state(app.clone(), auth::require_auth));

    let router = Router::new()
        .route("/", get(index))
        .route("/vendor/highlight.11.12.0.min.js", get(hljs_js))
        .route("/api/login", post(auth::login))
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
        // 常要看的点文件走白名单放行
        assert!(!tree_skip(".env.example"));
        assert!(!tree_skip(".gitignore"));
        // 但 .env 本体是密钥，不放
        assert!(tree_skip(".env"));
    }

    #[test]
    fn 依赖和构建目录也过滤() {
        for d in ["node_modules", "vendor", "dist", "build", "out", "__pycache__", "venv", ".venv"] {
            assert!(tree_skip(d), "{d} 该被过滤");
        }
        // 名字里含这些词但不完全相等的不误伤
        assert!(!tree_skip("outbox"));
        assert!(!tree_skip("dist_config"));
        assert!(!tree_skip("build.rs"));
    }

    #[test]
    fn 文件路径穿越挡在门外() {
        // 相对路径、每段干净，放行
        assert!(file_path_ok("src/main.rs"));
        assert!(file_path_ok("a/b/c.txt"));
        assert!(file_path_ok(".gitignore"));
        // 绝对路径、.. 穿越、空段，都拒
        assert!(!file_path_ok("/etc/passwd"));
        assert!(!file_path_ok("src/../Cargo.toml"));
        assert!(!file_path_ok("src//main.rs"));
        assert!(!file_path_ok(""));
        // 树里不显示的目录，拼 URL 也读不到
        assert!(!file_path_ok("node_modules/x/index.js"));
        assert!(!file_path_ok(".env"));
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
