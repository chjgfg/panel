// 一个只做四件事的小面板：看状态、看日志、启动、停止/重启。
// 网页是 static/ 下的三个文件，编译时直接嵌进二进制。
//
// 模块划分：
//   config   配置文件结构与查找
//   state    全局共享状态（会话表、采样缓存）
//   auth     登录/登出/会话校验
//   hostinfo 整机资源（/proc 与 df）
//   discover 项目发现（扫目录、黑名单、bin 探测）
//   systemd  systemctl/journalctl 交互
//   procs    面板外进程的探测与停止
//   srctree  源码目录树与文件读取
// 这里只剩 web 层：路由、handler 组装。
mod auth;
mod config;
mod discover;
mod hostinfo;
mod procs;
mod srctree;
mod state;
mod systemd;
mod terminal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::{BoxErr, Config, config_path};
use crate::discover::{bins, discover, find_cargo, ok_name};
use crate::procs::{find_outside, proc_exes, stop_pids};
use crate::state::App;
use crate::srctree::{Node, file_path_ok, read_file, walk_tree};
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

// 网页控制台的终端库 xterm.js，同样本地 vendored、编译时嵌进二进制，部署不依赖外网。
// 文件名带版本号，长缓存 immutable（no_cache 中间件对 /vendor/ 放行）。
async fn xterm_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_str!("../static/vendor/xterm.5.5.0.min.js"),
    )
}

async fn xterm_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_str!("../static/vendor/xterm.5.5.0.min.css"),
    )
}

// fit addon：把终端自适应铺满弹窗
async fn xterm_fit_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_str!("../static/vendor/xterm-addon-fit.0.10.0.min.js"),
    )
}

// 页面自己的 CSS/JS：也是编译时嵌进二进制的（部署仍只拷一个可执行文件）。
// 和 vendor 不同，它们跟页面同版本演进，设短缓存 + 升级换 v= 版本号。
async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("../static/app.js"),
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

/// 项目目录树（只给结构，内容点开文件时另走 /file）
async fn tree(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    let Some((name, dir)) = resolve(&app, &key).await else {
        return (StatusCode::NOT_FOUND, "未知项目").into_response();
    };
    let children = walk_tree(&dir).await;
    Json(Node { name, path: String::new(), dir: true, big: false, children }).into_response()
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
    Json(read_file(&dir.join(path)).await).into_response()
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

/// 前缀清洗：去掉首尾的 /，拒绝 "。"、空串等。返回 None = 不合法（空前缀）。
/// 空 prefix（配置里没写）走调用方的「挂在根路径」分支，不进这里。
fn clean_prefix(raw: &str) -> Result<String, String> {
    let p = raw.trim_matches('/');
    if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!(
            "prefix 只能由字母、数字、连字符组成（当前值 \"{raw}\"）。\
             随便生成一个：openssl rand -hex 8"
        ));
    }
    Ok(p.to_string())
}

/// 面板挂在秘密前缀下时，前缀之外的任何路径一律 404——
/// 扫描器看到的和一台空机器没有区别，连登录页都摸不到。
/// 唯一的善意例外：/前缀（无尾斜杠）301 到 /前缀/，浏览器输网址方便。
async fn secret_path(prefix: Arc<String>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == format!("/{prefix}") {
        let loc = format!("/{prefix}/");
        return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, loc)]).into_response();
    }
    next.run(req).await
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
    let prefix = cfg.prefix.trim().to_string();
    // App 拿走 cfg 所有权，之后想再读配置就从 App 里读
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
        .route("/api/terminal", get(terminal::terminal_ws))
        .layer(middleware::from_fn_with_state(app.clone(), auth::require_auth));

    let panel = Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(style_css))
        .route("/static/app.js", get(app_js))
        .route("/vendor/highlight.11.12.0.min.js", get(hljs_js))
        .route("/vendor/xterm.5.5.0.min.js", get(xterm_js))
        .route("/vendor/xterm.5.5.0.min.css", get(xterm_css))
        .route("/vendor/xterm-addon-fit.0.10.0.min.js", get(xterm_fit_js))
        .route("/api/login", post(auth::login))
        .merge(protected)
        .layer(middleware::from_fn(no_cache))
        .with_state(app.clone());

    // 配置了秘密前缀就把整个面板挪到 /前缀/ 下，前缀外一律 404；
    // 没配 = 挂在根路径，行为和从前完全一样。
    // 用 nest_service 而不是 nest：axum 0.8 的 nest 匹配不了「/前缀/」
    // 这个带尾斜杠的首页路径（内层 route("/") 只有裸 / 才命中）
    let router = if prefix.is_empty() {
        panel
    } else {
        let prefix = Arc::new(clean_prefix(&prefix)?);
        Router::new()
            .nest_service(&format!("/{prefix}"), panel)
            .layer(middleware::from_fn(move |req, next| {
                secret_path(prefix.clone(), req, next)
            }))
    };

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
    if !prefix.is_empty() {
        // 提前校验过了，这里只是启动日志再报一遍方便对账
        println!("秘密路径: /{}/ （其余路径一律 404）", clean_prefix(&prefix)?);
    }
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端验证前缀路由：/前缀（301 补斜杠）、/前缀/（首页）、
    /// 前缀外（404，扫描器看不出这里有面板）
    #[tokio::test]
    async fn 前缀外一律404前缀内正常() {
        use tower::ServiceExt;

        let inner = Router::new()
            .route("/", get(|| async { "HOME" }))
            .route("/api/login", get(|| async { "LOGIN" }));
        let prefix = "s3cret";
        let nest_at = format!("/{prefix}");
        let holder = Arc::new(prefix.to_string());
        let app: Router = Router::new()
            .nest_service(&nest_at, inner)
            .layer(middleware::from_fn(move |req, next| {
                secret_path(holder.clone(), req, next)
            }));

        for (path, want) in [
            ("/".to_string(), StatusCode::NOT_FOUND),
            ("/admin".to_string(), StatusCode::NOT_FOUND),
            ("/api/login".to_string(), StatusCode::NOT_FOUND),
            ("/static/style.css".to_string(), StatusCode::NOT_FOUND),
            (format!("/{prefix}"), StatusCode::MOVED_PERMANENTLY),
            (format!("/{prefix}/"), StatusCode::OK),
            (format!("/{prefix}/api/login"), StatusCode::OK),
        ] {
            let r = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path.clone())
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), want, "路径 {path}");
        }
    }

    #[test]
    fn 前缀清洗去掉斜杠并拒绝非法字符() {
        assert_eq!(clean_prefix("/abc/").unwrap(), "abc");
        assert_eq!(clean_prefix("a-b9").unwrap(), "a-b9");
        assert!(clean_prefix("a/b").is_err());
        assert!(clean_prefix("a b").is_err());
        assert!(clean_prefix("///").is_err());
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
