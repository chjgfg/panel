// 项目发现：扫目录、黑名单/通配排除、名字校验。
use std::path::PathBuf;

/// 文件夹名会拼成 unit 名交给 systemctl，所以只放行安全字符。
/// 开头是 - 会被当成命令行选项，开头是 . 的是隐藏目录（.git 之类）。
pub fn ok_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && !n.starts_with(['-', '.'])
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
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
}
