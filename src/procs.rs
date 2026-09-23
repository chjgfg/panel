// 面板外进程的探测与收尾：/proc 扫描、进程存活判断、TERM→KILL 停止序列。
use std::path::PathBuf;

use crate::systemd::run;

/// 扫一遍 /proc 拿到所有进程的 exe 路径。每次刷新只扫一次，
/// 再拿去跟各个项目目录比对，避免 N 个项目扫 N 遍 /proc。
/// 非 Linux（或 /proc 读不到）时返回空表，功能自动退化成「看不见外部进程」。
pub async fn proc_exes() -> Vec<(u32, PathBuf)> {
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
pub fn find_outside(exes: &[(u32, PathBuf)], dir: &std::path::Path) -> Vec<u32> {
    let target = dir.join("target");
    exes.iter()
        .filter(|(_, exe)| exe.starts_with(&target))
        .map(|(pid, _)| *pid)
        .collect()
}

/// 进程还活着吗。僵尸进程要算死的：它已经放掉端口了，
/// 只是父进程还没回收，等它「消失」会白等 3 秒然后误报杀不掉。
pub fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| alive_from_stat(&s))
}

/// /proc/<pid>/stat 格式是 `pid (comm) S ...`，comm 里可能有空格和括号
/// （进程名就叫 `foo (bar)` 也是合法的），所以状态字段要从最后一个 ) 往后取。
pub fn alive_from_stat(stat: &str) -> bool {
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
}

/// 先 TERM，等它们真的退出（最多 3 秒），赖着不走的补一发 KILL。
/// 必须等：端口是进程被回收之后才释放的，发完信号就返回会撞上 AddrInUse。
pub async fn stop_pids(pids: &[u32]) -> (bool, String) {
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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    signal("-KILL").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_process_treated_as_dead() {
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
    fn only_processes_under_target_dir() {
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
}
