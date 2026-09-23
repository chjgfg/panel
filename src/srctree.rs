// 项目源码树：目录树结构、懒加载文件读取、路径安全检查。
use serde::Serialize;

/// 源码树里不放的名字：点开头的隐藏项（.git、.idea 之类）、编译产物和依赖目录。
/// 例外白名单：.env.example、.gitignore 这类常要看的点文件放行。
/// node_modules/vendor 这类目录一个就好几万文件，进来树就废了。
pub fn tree_skip(name: &str) -> bool {
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
pub const MAX_SRC_FILE: u64 = 512 * 1024;

#[derive(Serialize)]
pub struct Node {
    pub name: String,
    /// 相对项目根的路径，用 / 连接
    pub path: String,
    pub dir: bool,
    /// 文件超过 512KB：树里就标出来，前端点都不用点
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub big: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
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

/// 项目目录树（只给结构，内容点开文件时另走 /file，见 main.rs）
pub async fn walk_tree(dir: &std::path::Path) -> Vec<Node> {
    walk(dir, "").await
}

/// 懒加载取文件时的路径检查：必须是相对路径，不带 .. 穿越到项目外，
/// 且路径上每段都是树里会显示的名字（node_modules 之类就算拼 URL 也读不到）
pub fn file_path_ok(path: &str) -> bool {
    !path.starts_with('/')
        && path.split('/').all(|seg| !seg.is_empty() && seg != ".." && !tree_skip(seg))
}

#[derive(Serialize)]
pub struct FileBody {
    /// None = 二进制（不是合法 UTF-8）。大小上限在树里已标 big，这里再防一道
    pub content: Option<String>,
}

/// 读单个文件内容（路径已由调用方用 file_path_ok 核过）。
/// 大小超限或非 UTF-8（二进制）都返回 None。
pub async fn read_file(full: &std::path::Path) -> FileBody {
    let content = match tokio::fs::metadata(full).await {
        Ok(m) if m.is_file() && m.len() <= MAX_SRC_FILE => {
            // 按 UTF-8 读不进来就是二进制，照样不给内容
            tokio::fs::read(full).await.ok().and_then(|b| String::from_utf8(b).ok())
        }
        _ => None,
    };
    FileBody { content }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_filters_hidden_and_target() {
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
    fn deps_and_build_dirs_filtered() {
        for d in ["node_modules", "vendor", "dist", "build", "out", "__pycache__", "venv", ".venv"] {
            assert!(tree_skip(d), "{d} 该被过滤");
        }
        // 名字里含这些词但不完全相等的不误伤
        assert!(!tree_skip("outbox"));
        assert!(!tree_skip("dist_config"));
        assert!(!tree_skip("build.rs"));
    }

    #[test]
    fn path_traversal_blocked() {
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
}
