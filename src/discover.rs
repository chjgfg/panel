// 项目发现：扫目录、黑名单/通配排除、名字校验、项目 bin 探测。
use std::collections::HashMap;
use std::format;
use std::path::PathBuf;

use serde::Deserialize;

/// 文件夹名会拼成 unit 名交给 systemctl，所以只放行安全字符。
/// 开头是 - 会被当成命令行选项，开头是 . 的是隐藏目录（.git 之类）。
pub fn ok_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && !n.starts_with(['-', '.'])
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// 只支持 * 的通配匹配，够用又不用引依赖。没有 * 就是全等。
pub fn glob_match(pat: &str, s: &str) -> bool {
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
pub fn excluded(patterns: &[String], name: &str, dir: &std::path::Path) -> bool {
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
pub fn is_self(dir: &std::path::Path) -> bool {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return false;
    };
    dir.canonicalize().is_ok_and(|d| exe.starts_with(d))
}

/// 扫配置里的目录，每个子文件夹算一个项目，返回 (项目名, 绝对路径)。
/// 每次请求都重新扫，所以新建文件夹后刷新网页就能看到，不用重启面板。
pub async fn discover(dirs: &[String], exclude: &[String]) -> Vec<(String, PathBuf)> {
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

#[derive(Deserialize)]
struct CargoToml {
    package: Option<CargoPkg>,
    #[serde(default)]
    bin: Vec<CargoBin>,
}
#[derive(Deserialize)]
struct CargoPkg {
    name: String,
}

#[derive(Deserialize)]
struct CargoBin {
    name: String,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

async fn read_manifest(dir: &std::path::Path) -> Result<CargoToml, String> {
    let path = dir.join("Cargo.toml");
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    parse_manifest(&text, &path)
}

fn parse_manifest(text: &str, path: &std::path::Path) -> Result<CargoToml, String> {
    toml::from_str(text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))
}

impl CargoToml {
    fn required_features(&self, bin: &str) -> Vec<String> {
        self.bin
            .iter()
            .find(|target| target.name == bin)
            .map(|target| target.required_features.clone())
            .unwrap_or_default()
    }
}

/// 一次读配置，返回每个 Cargo bin 及其必要 feature；失败不能退成无门槛。
pub async fn cargo_targets(dir: &std::path::Path) -> Result<Vec<(String, Vec<String>)>, String> {
    let manifest = read_manifest(dir).await?;
    let features: HashMap<String, Vec<String>> = manifest
        .bin
        .iter()
        .map(|target| (target.name.clone(), manifest.required_features(&target.name)))
        .collect();
    Ok(bins_with_manifest(dir, Some(manifest))
        .await
        .into_iter()
        .map(|name| {
            let required = features.get(&name).cloned().unwrap_or_default();
            (name, required)
        })
        .collect())
}

/// 列出 Cargo 项目里的 bin：
///   src/main.rs        -> Cargo.toml 里的包名（cargo 就是这么命名默认 bin 的）
///   src/bin/foo.rs     -> foo
///   src/bin/foo/main.rs -> foo
///   [[bin]] name       -> 显式声明的名字，支持自定义源码路径
async fn bins_with_manifest(dir: &std::path::Path, manifest: Option<CargoToml>) -> Vec<String> {
    let mut v = Vec::new();

    if let Some(ct) = manifest {
        if dir.join("src/main.rs").is_file()
            && let Some(pkg) = ct.package
            && ok_name(&pkg.name)
        {
            v.push(pkg.name);
        }
        v.extend(
            ct.bin
                .into_iter()
                .map(|target| target.name)
                .filter(|n| ok_name(n)),
        );
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
pub fn find_cargo(explicit: Option<&str>) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_features_only_come_from_selected_bin() {
        let manifest = parse_manifest(
            r#"
                [package]
                name = "example"
                [features]
                unrelated = []
                http = []
                queue = []
                tls = []
                [[bin]]
                name = "plain"
                [[bin]]
                name = "empty"
                required-features = []
                [[bin]]
                name = "dashboard"
                path = "tools/dashboard.rs"
                required-features = ["http"]
                [[bin]]
                name = "worker"
                required-features = ["queue", "tls"]
            "#,
            std::path::Path::new("Cargo.toml"),
        )
        .unwrap();
        assert!(manifest.required_features("example").is_empty());
        assert!(manifest.required_features("plain").is_empty());
        assert!(manifest.required_features("empty").is_empty());
        assert_eq!(manifest.required_features("dashboard"), ["http"]);
        assert_eq!(manifest.required_features("worker"), ["queue", "tls"]);
    }

    #[test]
    fn invalid_manifest_reports_path_and_parse_error() {
        let path = std::path::Path::new("broken/Cargo.toml");
        for text in [
            "[[bin]",
            "[[bin]]\nname = 'worker'\nrequired-features = 'queue'",
            "[[bin]]\nname = 'worker'\nrequired-features = [123]",
        ] {
            let Err(error) = parse_manifest(text, path) else {
                panic!("无效配置必须返回错误");
            };
            assert!(error.contains("解析 broken/Cargo.toml 失败"), "{error}");
            assert!(error.contains("TOML parse error"), "{error}");
        }
    }

    #[tokio::test]
    async fn unreadable_manifest_fails_start_checks() {
        // 用已有文件充当目录，保证读取失败，同时无需创建临时产物。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let error = cargo_targets(&dir).await.unwrap_err();
        assert!(error.contains("读取"), "{error}");
        assert!(error.contains("Cargo.toml"), "{error}");
    }

    #[tokio::test]
    async fn explicit_bin_with_custom_path_is_discovered() {
        let manifest = parse_manifest(
            "[[bin]]\nname = 'custom-worker'\npath = 'tools/worker.rs'\nrequired-features = ['queue']",
            std::path::Path::new("Cargo.toml"),
        )
        .unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let names = bins_with_manifest(dir, Some(manifest)).await;
        assert!(names.iter().any(|name| name == "custom-worker"));
    }

    #[test]
    fn dir_name_allows_only_safe_chars() {
        assert!(ok_name("blog"));
        assert!(ok_name("my-api_2.0"));
        assert!(!ok_name("tpl@inst")); // @ 会被 systemd 当成模板单元的分隔符，unit 名里不能有
        assert!(!ok_name(""));
        assert!(!ok_name(".git")); // 隐藏目录
        assert!(!ok_name("-rf")); // 会被当成命令行选项
        assert!(!ok_name("a b")); // 空格
        assert!(!ok_name("../etc")); // 路径穿越
        assert!(!ok_name("naïve")); // 非 ASCII
        assert!(!ok_name(&"x".repeat(101)));
    }

    #[test]
    fn glob_only_supports_star() {
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
    fn blacklist_matches_by_name_or_path() {
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
}
