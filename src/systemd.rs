// systemctl / journalctl 交互：批量查状态、unit 命名规则、项目 unit 收拢。
use std::collections::HashMap;
use std::process::Stdio;

use tokio::process::Command;

pub const PROPS: &str = "Id,LoadState,ActiveState,SubState,UnitFileState,MainPID,\
                         MemoryCurrent,CPUUsageNSec,ActiveEnterTimestampMonotonic";

/// 一次 systemctl show 要查的属性之外的公共部分
#[derive(Clone, Default)]
pub struct Raw {
    pub load: String,
    pub active: String,
    pub sub: String,
    pub enabled: String,
    pub pid: u64,
    pub memory: Option<u64>,
    pub cpu_nsec: Option<u64>,
    pub active_since_us: Option<u64>,
}

/// systemd 对「未设置」的数值属性会返回 u64::MAX 或 [not set]，都要当 None
pub fn num(v: &str) -> Option<u64> {
    match v.parse::<u64>() {
        Ok(n) if n != u64::MAX => Some(n),
        _ => None,
    }
}

/// 注意是直接 exec，不经过 shell，所以参数里有什么字符都不会被解释
pub async fn run(cmd: &str, args: &[&str]) -> std::io::Result<(bool, String, String)> {
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
pub fn parse_show(block: &str) -> Raw {
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
pub async fn show_many(units: &[String]) -> HashMap<String, Raw> {
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
pub async fn boot_secs() -> f64 {
    tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

/// 每个运行实例对应一个独立的 transient unit，名字加 panel- 前缀避免撞上系统里的服务。
/// 一个项目会计着多个 bin 同时跑，所以 unit 名要带上 bin —— systemd-run 不允许两个
/// active 的 unit 同名，不带 bin 的话第二个 bin 就起不来了。
pub fn unit_of(project: &str, bin: &str) -> String {
    format!("panel-{project}-{bin}.service")
}

/// 一个项目在面板里可能占着的全部候选 unit：
///   每个 bin 一个面板 unit（panel-{p}-{bin}.service），
///   加上你可能自己写过的 {p}.service。
/// 刷新和 stop 都要把「这一个项目」跟 unit 群对上号。
pub fn project_units(project: &str, bins: &[String]) -> Vec<String> {
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
pub fn bin_of_panel_unit(project: &str, unit: &str) -> Option<String> {
    let prefix = format!("panel-{project}-");
    unit.strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(".service"))
        .map(str::to_string)
}

/// 一个项目在面板里可能同时跑着多个 bin，各自一个 panel-{p}-{bin} unit，
/// 也可能有你手写的 {p}.service。刷新时把「这一个项目」聚合成一个可见的行：
/// 挑一个 unit 当主 unit 报状态，并把「当前由面板起着、在跑的 bins」汇总出来
/// 给前端回显勾选。
pub struct Picked {
    /// 主 unit 名，展示和查日志用
    pub unit: String,
    /// 主 unit 是不是你自己写的 {p}.service（外部 unit，面板只做启停不选 bin）
    pub external: bool,
    pub raw: Raw,
    /// 当前由面板起着（active）的各 bin 名。前端靠它回显「这个项目现在跑着哪些 bin」
    pub running_bins: Vec<String>,
}

/// 从 show_many 的结果里把一个项目的全部 unit 收拢（只读，不改动 shown）。
/// 主 unit 挑选：面板的 bin unit 里谁在跑取谁；没在跑再退回你手写的 unit；
/// 都没有就按第一个面板 unit 报「未运行」。
pub fn pick_project(shown: &HashMap<String, Raw>, project: &str, bins: &[String]) -> Picked {
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
pub async fn pick_project_checked(project: &str, bins: &[String]) -> Picked {
    let cands = project_units(project, bins);
    let shown = show_many(&cands).await;
    pick_project(&shown, project, bins)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
