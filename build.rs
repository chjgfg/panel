// 编译时把「这是哪一版」钉进二进制：面板裸挂在服务器上跑，出问题时第一件事
// 就是搞清楚跑的到底是不是刚推的那份代码。页面上没有版本信息的话，只能靠
// 「我改的那个按钮在不在」来猜，成本很高。
use std::process::Command;

fn main() {
    // 只有源码变了才重新跑一遍 build.rs。否则这里输出的构建时间戳会让每次
    // cargo build 都算一次「有变化」，增量编译就白做了。
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=static");

    // 浅克隆 / 打包上传时可能没有 .git，拿不到就当未知，不能让编译失败
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "未知".into());
    // 有没有未提交的改动：出现过「本地改了没推就跑起来」的情况，标出来最好查
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("cargo:rustc-env=PANEL_GIT={sha}{}", if dirty { "-dirty" } else { "" });
    println!("cargo:rustc-env=PANEL_BUILT={now}");
}
