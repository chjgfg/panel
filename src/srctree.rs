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

// ---------- 跨文件搜索 ----------

/// 一次搜索的三条预算。单核机器上「搜一下」不该把面板卡住，也不该返回几千行
/// 把浏览器和响应体一起撑爆：文件数、字节数、命中数各封一道，超了标 truncated。
pub const SEARCH_MAX_FILES: usize = 2000;
pub const SEARCH_MAX_BYTES: u64 = 24 * 1024 * 1024;
pub const SEARCH_MAX_HITS: usize = 300;
/// 结果里一行的字符上限：min.js 那种一行几万字节的归档文件，整行塞进响应没意义
pub const SEARCH_LINE_MAX: usize = 160;
/// 命中行号用 0 表示「是文件名匹配上的，不是正文」
pub const HIT_BY_NAME: usize = 0;

#[derive(Serialize)]
pub struct Hit {
    /// 相对项目根的路径，和树里的 path 同一套写法（前端能直接拿去走 /file）
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Serialize)]
pub struct SearchResp {
    pub hits: Vec<Hit>,
    /// 有命中的文件数
    pub files: usize,
    /// 真打开读过的文件数 —— 让前端能说清「扫了 812 个文件」而不是假装扫全了
    pub scanned: usize,
    pub truncated: bool,
    pub q: String,
}

/// 结果里只留每行去掉缩进后的一段。缩进在结果列表里没有意义（一行一条，
/// 不像正文要靠它看层级），截断则按字符切，中文路径和注释不会被切成半个字
fn brief(l: &str) -> String {
    l.trim_start().chars().take(SEARCH_LINE_MAX).collect()
}

/// 摊平目录树成 (相对路径, 绝对路径)。和 walk 同一套过滤：隐藏项、编译产物、
/// 依赖目录都不进；符号链接不跟（跟了会有环）。
fn collect(dir: &std::path::Path, rel: &str, depth: usize, out: &mut Vec<(String, std::path::PathBuf)>) {
    // 深度也封一道：真出现 20 层嵌套时，宁可少扫也别把栈吃穿
    if depth > 12 || out.len() >= SEARCH_MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    // 排序保证同一个查询两次得到同一份结果（read_dir 的顺序不保证）
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= SEARCH_MAX_FILES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if tree_skip(&name) {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if ft.is_dir() {
            collect(&e.path(), &path, depth + 1, out);
        } else if ft.is_file() {
            out.push((path, e.path()));
        }
    }
}

/// 在项目目录里搜一个子串（忽略大小写）。正文命中记行号，文件名命中记
/// HIT_BY_NAME，两者都给同一份结果列表。
/// 同步函数：调用方负责丢给 spawn_blocking（见 main.rs 的 search）。
pub fn search_dir(dir: &std::path::Path, q: &str) -> SearchResp {
    let needle = q.to_lowercase();
    let mut files = Vec::new();
    collect(dir, "", 0, &mut files);

    let mut hits: Vec<Hit> = Vec::new();
    let mut hit_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut scanned = 0usize;
    let mut bytes = 0u64;
    let mut truncated = false;

    for (rel, full) in files {
        if hits.len() >= SEARCH_MAX_HITS {
            truncated = true;
            break;
        }
        let name_hit = rel.to_lowercase().contains(&needle);
        let Ok(md) = std::fs::metadata(&full) else { continue };
        // 比阅读上限还大的文件树里已标过，这里同样不搜
        if !md.is_file() || md.len() > MAX_SRC_FILE {
            if name_hit {
                hits.push(Hit { path: rel, line: HIT_BY_NAME, text: String::new() });
            }
            continue;
        }
        if bytes + md.len() > SEARCH_MAX_BYTES {
            truncated = true;
            break;
        }
        let Ok(data) = std::fs::read(&full) else { continue };
        bytes += md.len();
        scanned += 1;
        let Ok(text) = String::from_utf8(data) else {
            // 二进制不搜正文；名字能匹配上还是给一条，不然搜 .png 会一条都没有
            if name_hit {
                hits.push(Hit { path: rel, line: HIT_BY_NAME, text: String::new() });
            }
            continue;
        };
        let mut got = false;
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                got = true;
                hits.push(Hit { path: rel.clone(), line: i + 1, text: brief(line) });
                if hits.len() >= SEARCH_MAX_HITS {
                    truncated = true;
                    break;
                }
            }
        }
        if !got && name_hit {
            hits.push(Hit { path: rel.clone(), line: HIT_BY_NAME, text: String::new() });
        }
        if got || name_hit {
            hit_files.insert(rel);
        }
    }
    SearchResp { hits, files: hit_files.len(), scanned, truncated, q: q.to_string() }
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

    /// 在系统临时目录里搭一棵小树。tag 各测试不同名：并行跑时不能互相踩
    fn tmp_tree(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("panel-search-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn search_finds_lines_case_insensitively() {
        let dir = tmp_tree("hits", &[
            ("src/main.rs", "fn main() {\n    let k = Keycode::new();\n}\n"),
            ("src/key.rs", "// KEYCODE 表\nstruct Foo;\n"),
        ]);
        let r = search_dir(&dir, "keycode");
        assert_eq!(r.files, 2, "两个文件都该有份");
        assert_eq!(r.scanned, 2);
        assert!(!r.truncated);
        // 大小写不敏感：主文件里是小写、key.rs 里是全大写
        let lines: Vec<(String, usize)> =
            r.hits.iter().map(|h| (h.path.clone(), h.line)).collect();
        assert_eq!(lines, vec![("src/key.rs".to_string(), 1usize), ("src/main.rs".to_string(), 2usize)]);
        // 行号从 1 起，正文按原样给（前端要显示那一行长什么样）
        let first = &r.hits[0];
        assert_eq!(first.text, "// KEYCODE 表");
        assert_eq!(r.q, "keycode");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_skips_hidden_build_and_deps() {
        let dir = tmp_tree("skip", &[
            ("src/a.rs", "needle\n"),
            ("target/out.rs", "needle\n"),
            ("node_modules/x/index.js", "needle\n"),
            (".git/config.rs", "needle\n"),
        ]);
        let r = search_dir(&dir, "needle");
        assert_eq!(r.scanned, 1, "只该扫到 src/a.rs");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].path, "src/a.rs");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_reports_name_only_match_and_ignores_binary_body() {
        let dir = std::env::temp_dir().join("panel-search-nameonly");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // 文件名对得上，正文里一次都没出现
        std::fs::write(dir.join("src/keymap.rs"), "fn a() {}\n").unwrap();
        // 二进制：非 UTF-8，正文不搜，但名字命中还是要给（搜 .png 不该空手而归）
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/logo.png"), [0u8, 159, 2, 1, 254, 253]).unwrap();

        let r = search_dir(&dir, "logo.png");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].line, HIT_BY_NAME, "文件名命中用 0 表示");
        assert_eq!(r.hits[0].path, "assets/logo.png");
        assert_eq!(r.scanned, 2, "两个文件都打开过，只是二进制那个不搜正文");

        let k = search_dir(&dir, "keymap");
        assert_eq!(k.hits[0].line, HIT_BY_NAME);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_stops_at_budget_and_says_so() {
        // 一个文件里塞超出命中上限的行数：该收手并标 truncated
        let many = "needle\n".repeat(SEARCH_MAX_HITS + 50);
        let dir = tmp_tree("budget", &[("big.txt", &many), ("after.txt", "needle\n")]);
        let r = search_dir(&dir, "needle");
        assert_eq!(r.hits.len(), SEARCH_MAX_HITS);
        assert!(r.truncated, "撞到上限必须说，不能装作搜全了");
        // 结果行去掉缩进，且不超过单行上限
        assert!(r.hits.iter().all(|h| h.text.chars().count() <= SEARCH_LINE_MAX));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
