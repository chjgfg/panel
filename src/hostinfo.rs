// 整机资源：/proc 与 df 的解析、host_stats 的聚合。
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::state::App;
use crate::systemd::run;

#[derive(Serialize, Clone)]
pub struct Disk {
    pub mount: String,
    pub used: u64,
    pub total: u64,
}

#[derive(Serialize)]
pub struct Host {
    /// 整机 CPU 占用百分比，首次请求拿不到（要两次采样做差）
    pub cpu: Option<f64>,
    pub cores: usize,
    pub load: [f64; 3],
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disks: Vec<Disk>,
    pub uptime: f64,
}

/// /proc/stat 第一行 `cpu  user nice system idle iowait ...` 是开机以来的累计
/// 时间片，两次采样做差才是占用率。idle 和 iowait 都算「没在干活」。
/// 返回 (忙碌, 总计)。
pub fn parse_cpu_line(s: &str) -> Option<(u64, u64)> {
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
pub fn parse_meminfo(s: &str) -> HashMap<&str, u64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let n: u64 = v.split_whitespace().next()?.parse().ok()?;
            Some((k, n * 1024))
        })
        .collect()
}

pub fn parse_loadavg(s: &str) -> [f64; 3] {
    let mut out = [0.0; 3];
    for (i, x) in s.split_whitespace().take(3).enumerate() {
        out[i] = x.parse().unwrap_or(0.0);
    }
    out
}

/// 解析 `df -kP` 的输出。用 used+avail 当总量而不是第二列的 1K-blocks：
/// 后者含保留块，跟 df 自己算 Use% 的分母不一致，会显得对不上。
pub fn parse_df(s: &str) -> Vec<Disk> {
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

pub async fn host_stats(app: &App) -> Host {
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
        uptime: crate::systemd::boot_secs().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_cpu_diff_of_two_samples() {
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
    fn memory_used_from_available() {
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
    fn loadavg_takes_first_three() {
        assert_eq!(
            parse_loadavg("0.42 0.30 0.25 1/234 5678\n"),
            [0.42, 0.30, 0.25]
        );
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn df_output_parsed_and_mounts_deduped() {
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
}
