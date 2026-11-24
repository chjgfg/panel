// 自动入口只读项目声明和源码，不执行包管理器，不安装依赖，也不构建。
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::{format, matches, vec};

use serde::Deserialize;

use crate::discover::{cargo_targets, ok_name};
use crate::launch::{CommandTarget, FileSource, ProjectTargets, Target, read_config};

async fn exists(path: &Path) -> Result<bool, String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("检查 {} 失败：{e}", path.display())),
    }
}

pub(crate) async fn entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !exists(path).await? {
        return Ok(vec![]);
    }
    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(|e| format!("读取目录 {} 失败：{e}", path.display()))?;
    let mut paths = vec![];
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| format!("读取目录 {} 失败：{e}", path.display()))?
    {
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

pub async fn discover_targets(dir: &Path) -> Result<ProjectTargets, String> {
    let cargo = exists(&dir.join("Cargo.toml")).await?;
    let mut targets: Vec<Target> = if cargo {
        cargo_targets(dir)
            .await?
            .into_iter()
            .map(|(name, features)| Target::Cargo { name, features })
            .collect()
    } else {
        vec![]
    };
    let mut other = false;
    let node_path = dir.join("package.json");
    if let FileSource::Present(text) = read_config(&node_path).await? {
        other = true;
        let package = parse_node(&text, &node_path)?;
        let mut locks = BTreeSet::new();
        for (file, manager) in [
            ("package-lock.json", "npm"),
            ("npm-shrinkwrap.json", "npm"),
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lock", "bun"),
            ("bun.lockb", "bun"),
        ] {
            if exists(&dir.join(file)).await? {
                locks.insert(manager);
            }
        }
        targets.extend(node_targets(package, &locks, &node_path)?);
    }
    let python_path = dir.join("pyproject.toml");
    let mut python_declared = false;
    if let FileSource::Present(text) = read_config(&python_path).await? {
        other = true;
        let scripts = python_scripts(&text, &python_path)?;
        if !scripts.is_empty() {
            python_declared = true;
            let interpreter = python_interpreter(dir).await?;
            targets.extend(python_targets(scripts, &interpreter)?);
        }
    }
    if !python_declared {
        let scripts = python_files(dir).await?;
        other |= !scripts.is_empty();
        targets.extend(scripts);
    }
    let java_builds = crate::java_launch::discover(dir).await?;
    other |= !java_builds.is_empty();
    targets.extend(java_builds);
    let jars = java_targets(dir).await?;
    other |= !jars.is_empty();
    targets.extend(jars);
    let go_path = dir.join("go.mod");
    if let FileSource::Present(text) = read_config(&go_path).await? {
        other = true;
        check_go_module(&text, &go_path)?;
        targets.extend(go_targets(dir).await?);
    }
    let mut names = BTreeSet::new();
    for target in &targets {
        if !names.insert(target.name()) {
            return Err(format!(
                "项目 {} 的自动入口名称冲突：{}，请用 panel-run.toml 明确配置",
                dir.display(),
                target.name()
            ));
        }
    }
    targets.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(
        if cargo
            && !targets
                .iter()
                .any(|target| matches!(target, Target::Command(_)))
        {
            ProjectTargets::Cargo(targets)
        } else if other {
            ProjectTargets::Discovered(targets)
        } else {
            ProjectTargets::Unconfigured
        },
    )
}

fn command(language: &str, name: &str, program: &str, args: Vec<String>) -> Result<Target, String> {
    Ok(Target::Command(CommandTarget {
        name: entry_name(language, name)?,
        program: program.into(),
        args,
        working_dir: ".".into(),
        env: BTreeMap::new(),
    }))
}

async fn python_files(dir: &Path) -> Result<Vec<Target>, String> {
    let mut files = vec![];
    // 只列根目录约定入口，不把任意库模块当作可运行程序。
    for file in ["main.py", "app.py", "__main__.py"] {
        if let FileSource::Present(_) = read_config(&dir.join(file)).await? { files.push(file); }
    }
    if files.is_empty() { return Ok(vec![]); }
    let interpreter = python_interpreter(dir).await?;
    files.into_iter().map(|file| command("python", file, &interpreter, vec!["-u".into(), file.into()])).collect()
}

fn entry_name(language: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains('\0') {
        return Err(format!("{language} 的入口名称不能为空或包含 NUL"));
    }
    // 下划线本身也转义，保证 a:b 和 a_3ab 不会合并为同一个 unit。
    let mut encoded = format!("{language}-");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b".-".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("_{byte:02x}"));
        }
    }
    if !ok_name(&encoded) {
        return Err(format!(
            "自动入口名称过长：{name}，请用 panel-run.toml 配置短名称"
        ));
    }
    Ok(encoded)
}

#[derive(Deserialize)]
struct NodePackage {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
}

fn parse_node(text: &str, path: &Path) -> Result<NodePackage, String> {
    serde_json::from_str(text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))
}

fn node_targets(
    package: NodePackage,
    locks: &BTreeSet<&str>,
    path: &Path,
) -> Result<Vec<Target>, String> {
    let startup = |name: &str| matches!(name.split(':').next(), Some("start" | "dev" | "serve"));
    // 只有构建脚本的项目不需要选择启动管理器，也不能改变纯 Cargo 项目的服务规则。
    if !package.scripts.keys().any(|name| startup(name)) {
        return Ok(vec![]);
    }
    let manager = if let Some(declared) = &package.package_manager {
        let (name, version) = declared.split_once('@').unwrap_or((declared, ""));
        if !matches!(name, "npm" | "pnpm" | "yarn" | "bun") || version.trim().is_empty() {
            return Err(format!(
                "{} 的 packageManager 不支持：{declared}",
                path.display()
            ));
        }
        name
    } else if locks.len() > 1 {
        return Err(format!(
            "{} 存在多个包管理器的锁文件，请声明 packageManager 或 panel-run.toml",
            path.display()
        ));
    } else {
        locks.first().copied().unwrap_or("npm")
    };
    let mut targets = vec![];
    for (name, script) in package.scripts {
        if !startup(&name) {
            continue;
        }
        if script.trim().is_empty() || script.contains('\0') {
            return Err(format!("{} 的脚本 {name} 为空或包含 NUL", path.display()));
        }
        let mut args = vec!["run".into(), name.clone()];
        // npm 的分隔符由 npm 消耗；其他三个管理器直接转发脚本名后的参数。
        if manager == "npm" {
            args.push("--".into());
        }
        targets.push(command("node", &name, manager, args)?);
    }
    Ok(targets)
}

#[derive(Default, Deserialize)]
struct PythonProject {
    #[serde(default)]
    project: PythonScripts,
    #[serde(default)]
    tool: PythonTools,
}

#[derive(Default, Deserialize)]
struct PythonScripts {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
struct PythonTools {
    #[serde(default)]
    poetry: PoetryScripts,
}

#[derive(Default, Deserialize)]
struct PoetryScripts {
    #[serde(default)]
    scripts: BTreeMap<String, PoetryScript>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PoetryScript {
    Reference(String),
    Table {
        reference: String,
        r#type: Option<String>,
    },
}

fn python_scripts(text: &str, path: &Path) -> Result<BTreeMap<String, String>, String> {
    let config: PythonProject =
        toml::from_str(text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    let mut scripts = config.project.scripts;
    for (name, script) in config.tool.poetry.scripts {
        let reference = match script {
            PoetryScript::Reference(reference) => reference,
            PoetryScript::Table { reference, r#type } => {
                if r#type.as_deref().is_some_and(|t| t != "console") {
                    return Err(format!(
                        "{} 的 Poetry 脚本 {name} 不是 console 入口，请用 panel-run.toml 配置",
                        path.display()
                    ));
                }
                reference
            }
        };
        if scripts
            .get(&name)
            .is_some_and(|existing| existing != &reference)
        {
            return Err(format!("{} 中脚本 {name} 的两处声明冲突", path.display()));
        }
        scripts.insert(name, reference);
    }
    for (name, reference) in &scripts {
        if python_reference(reference).is_none() {
            return Err(format!(
                "{} 的脚本 {name} 必须是 module:callable：{reference}",
                path.display()
            ));
        }
    }
    Ok(scripts)
}

fn python_reference(reference: &str) -> Option<(&str, &str)> {
    let (module, callable) = reference.split_once(':')?;
    let (module, callable) = (module.trim(), callable.trim());
    let dotted_identifier = |s: &str| {
        s.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    (dotted_identifier(module) && dotted_identifier(callable)).then_some((module, callable))
}

async fn python_interpreter(dir: &Path) -> Result<String, String> {
    let venv = dir.join(".venv");
    if exists(&venv).await? {
        let relative = if cfg!(windows) {
            ".venv/Scripts/python.exe"
        } else {
            ".venv/bin/python"
        };
        if !exists(&dir.join(relative)).await? {
            return Err(format!("虚拟环境 {} 缺少 Python 解释器", venv.display()));
        }
        Ok(relative.into())
    } else {
        Ok(if cfg!(windows) { "python" } else { "python3" }.into())
    }
}

fn python_targets(
    scripts: BTreeMap<String, String>,
    interpreter: &str,
) -> Result<Vec<Target>, String> {
    scripts.into_iter().map(|(name, reference)| {
        let (module, callable) = python_reference(&reference)
            .ok_or_else(|| format!("Python 脚本 {name} 的引用非法：{reference}"))?;
        let literal = |s: &str| serde_json::to_string(s).expect("字符串可序列化");
        let code = format!(
            "import importlib, functools, sys; sys.argv[0] = {}; sys.exit(functools.reduce(getattr, {}.split('.'), importlib.import_module({}))())",
            literal(&name), literal(callable), literal(module)
        );
        command("python", &name, interpreter, vec!["-u".into(), "-c".into(), code])
    }).collect()
}

fn jar_runnable(reader: impl Read + Seek, path: &Path) -> Result<bool, String> {
    let bad = |e| format!("读取 JAR {} 的清单失败：{e}", path.display());
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| bad(e.to_string()))?;
    let manifest = match zip.by_name("META-INF/MANIFEST.MF") {
        Ok(manifest) => manifest,
        Err(zip::result::ZipError::FileNotFound) => return Ok(false),
        Err(e) => return Err(bad(e.to_string())),
    };
    // 只解压清单并限制长度，发现入口不会把整个 JAR 读进内存。
    let mut text = String::new();
    manifest
        .take(65537)
        .read_to_string(&mut text)
        .map_err(|e| bad(e.to_string()))?;
    if text.len() > 65536 {
        return Err(bad("清单超过 64 KiB".into()));
    }
    let mut headers = BTreeMap::<String, String>::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(continuation) = line.strip_prefix(' ') {
            let value = headers
                .get_mut(&current)
                .ok_or_else(|| bad("清单续行缺少字段".into()))?;
            value.push_str(continuation);
        } else {
            let (key, value) = line
                .split_once(": ")
                .ok_or_else(|| bad("清单字段格式非法".into()))?;
            current = key.to_ascii_lowercase();
            if headers.insert(current.clone(), value.into()).is_some() {
                return Err(bad(format!("清单字段重复：{key}")));
            }
        }
    }
    Ok(headers
        .get("main-class")
        .is_some_and(|value| !value.trim().is_empty()))
}

pub(crate) async fn java_targets(dir: &Path) -> Result<Vec<Target>, String> {
    let mut targets = vec![];
    for relative in [".", "target", "build/libs"] {
        for path in entries(&dir.join(relative)).await? {
            if path.extension().is_none_or(|e| e != "jar") {
                continue;
            }
            let jar_path = path.clone();
            let runnable = tokio::task::spawn_blocking(move || {
                let file = std::fs::File::open(&jar_path)
                    .map_err(|e| format!("读取 {} 失败：{e}", jar_path.display()))?;
                jar_runnable(file, &jar_path)
            })
            .await
            .map_err(|e| format!("读取 JAR 任务失败：{e}"))??;
            if runnable {
                let name = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let name = name.strip_prefix("./").unwrap_or(&name);
                targets.push(command(
                    "java",
                    name,
                    "java",
                    // JAR 路径相对于启动工作目录，不能重复带入扫描目录。
                    vec!["-jar".into(), name.into()],
                )?);
            }
        }
    }
    Ok(targets)
}

fn check_go_module(text: &str, path: &Path) -> Result<(), String> {
    let modules: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let line = line.split("//").next().unwrap_or("").trim();
            let mut words = line.split_whitespace();
            (words.next() == Some("module")).then(|| words.collect::<Vec<_>>())
        })
        .collect();
    if modules.len() != 1
        || modules[0].len() != 1
        || modules[0][0].contains('\0')
        || modules[0][0].trim_matches('"').is_empty()
        || modules[0][0].starts_with('"') != modules[0][0].ends_with('"')
    {
        return Err(format!("{} 必须包含唯一的 module 路径声明", path.display()));
    }
    Ok(())
}

// 跳过注释和三种字符串，避免说明文字中的 package main / func main 被当作入口。
fn go_main(text: &str) -> Result<bool, String> {
    let bytes = text.as_bytes();
    let mut tokens = vec![];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && &bytes[i..i + 2] != b"*/" {
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return Err("Go 块注释未结束".into());
                }
                i += 2;
            }
            quote @ (b'"' | b'\'' | b'`') => {
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        closed = true;
                        break;
                    }
                    if bytes[i] == b'\\' && quote != b'`' {
                        i += 1;
                    }
                    i += 1;
                }
                if !closed {
                    return Err("Go 字符串未结束".into());
                }
                tokens.push("literal");
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push(&text[start..i]);
            }
            b'(' => {
                tokens.push("(");
                i += 1;
            }
            b')' => {
                tokens.push(")");
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                i += 1;
            }
            _ => {
                tokens.push("other");
                i += 1;
            }
        }
    }
    Ok(tokens.windows(2).any(|t| t == ["package", "main"])
        && tokens.windows(4).any(|t| t == ["func", "main", "(", ")"]))
}

async fn go_targets(dir: &Path) -> Result<Vec<Target>, String> {
    let mut targets = vec![];
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    if exists(&dir.join("cmd")).await? {
        pending.push((dir.join("cmd"), 1));
    }
    while let Some((package, depth)) = pending.pop() {
        // 嵌套 module 由自己的项目配置管理，不在父 module 下生成错误的 go run 路径。
        if depth > 0 && exists(&package.join("go.mod")).await? {
            continue;
        }
        let mut main = false;
        for path in entries(&package).await? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let metadata = tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
            if depth > 0 && metadata.is_dir() && !name.starts_with(['.', '_']) && name != "vendor" {
                if depth >= 16 {
                    return Err(format!("Go cmd 目录层级过深：{}", path.display()));
                }
                pending.push((path, depth + 1));
                continue;
            }
            if path.extension().is_some_and(|e| e == "go")
                && !name.ends_with("_test.go")
                && !name.starts_with(['.', '_'])
                && !metadata.is_dir()
            {
                let text = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
                main |= go_main(&text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
            }
        }
        if main {
            let relative = package
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let (name, argument) = if relative.is_empty() {
                ("main", ".".into())
            } else {
                (relative.as_str(), format!("./{relative}"))
            };
            targets.push(command("go", name, "go", vec!["run".into(), argument])?);
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{load_targets, systemd_run_argv};
    use std::io::{Cursor, Write};

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/launch")
    }

    #[tokio::test]
    async fn python_files_are_fallback_entries_and_keep_sources_and_literal_args() {
        let dir = fixture().with_file_name("python-files");
        let targets = load_targets(&dir).await.unwrap();
        assert_eq!(targets.names(), ["python-_5f_5fmain_5f_5f.py", "python-app.py", "python-main.py"]);
        let extra = "  literal \"quote\"\nnext\t ";
        let mut spec = spec(targets.target("python-main.py").unwrap()).clone();
        spec.program = std::env::current_exe().unwrap().display().to_string();
        let simulated = ProjectTargets::Discovered(vec![Target::Command(spec)]);
        assert_eq!(simulated.target("python-main.py").unwrap().prepare(&dir,extra,None).unwrap().args, ["-u", "main.py", extra]);
        assert_eq!(simulated.preview(&dir,"python-main.py",extra,None).unwrap().source, "main.py");
        assert_eq!(python_files(&fixture()).await.unwrap().len(), 0);
        assert_eq!(load_targets(&fixture()).await.unwrap().names().iter().filter(|n| n.starts_with("python-")).count(), 1);
    }

    #[tokio::test]
    #[ignore = "需要 Python 运行时"]
    async fn real_python_file_receives_literal_argument() {
        let dir = fixture().with_file_name("python-files");
        let targets = load_targets(&dir).await.unwrap();
        let extra = "  --label \"two words\" 'single'\nnext\t ";
        for name in targets.names() {
            let command = targets.target(&name).unwrap().prepare(&dir,extra,None).unwrap();
            let output = tokio::process::Command::new(&command.program).args(&command.args).envs(&command.env)
                .env("PANEL_EXPECT_ARGUMENT",extra).env("PYTHONDONTWRITEBYTECODE","1").current_dir(&dir).output().await.unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("PYTHON_FILE_OK"));
        }
    }

    #[tokio::test]
    #[ignore = "需要 Go 工具链"]
    async fn real_go_entry_receives_literal_argument() {
        let dir = fixture().with_file_name("go-probe");
        let targets = load_targets(&dir).await.unwrap();
        let extra = "  --label \"two words\" 'single'\nnext\t ";
        let command = targets.target("go-main").unwrap().prepare(&dir,extra,None).unwrap();
        let output = tokio::process::Command::new(&command.program).args(&command.args).envs(&command.env)
            .env("PANEL_EXPECT_ARGUMENT",extra).current_dir(&dir).output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("GO_LAUNCH_OK"));
    }

    fn spec(target: &Target) -> &CommandTarget {
        match target {
            Target::Command(spec) => spec,
            _ => panic!("应为通用命令"),
        }
    }

    fn node(manager: Option<&str>, locks: &[&str]) -> Result<Vec<Target>, String> {
        node_targets(
            NodePackage {
                package_manager: manager.map(String::from),
                scripts: BTreeMap::from([
                    ("start".into(), "node app.js".into()),
                    ("dev:api".into(), "node app.js".into()),
                    ("serve".into(), "node app.js".into()),
                    ("build".into(), "build-tool".into()),
                    ("restart".into(), "restart-tool".into()),
                ]),
            },
            &locks.iter().copied().collect(),
            Path::new("package.json"),
        )
    }

    #[test]
    fn node_managers_filter_scripts_and_use_their_argument_boundaries() {
        for manager in ["npm", "pnpm", "yarn", "bun"] {
            let targets = node(None, &[manager]).unwrap();
            assert_eq!(targets.len(), 3);
            let target = targets.iter().find(|t| t.name() == "node-start").unwrap();
            assert_eq!(spec(target).program, manager);
            let mut expected = vec!["run", "start"];
            if manager == "npm" {
                expected.push("--");
            }
            assert_eq!(spec(target).args, expected);
        }
        assert_eq!(spec(&node(None, &[]).unwrap()[0]).program, "npm");
        assert_eq!(
            spec(&node(Some("pnpm@9.1.0"), &["npm", "yarn"]).unwrap()[0]).program,
            "pnpm"
        );
    }

    #[test]
    fn node_invalid_configuration_and_ambiguous_locks_are_errors() {
        for manager in ["unknown@1", "npm", "npm@", "pnpm@   "] {
            assert!(
                node(Some(manager), &[])
                    .unwrap_err()
                    .contains("packageManager")
            );
        }
        assert!(node(None, &["npm", "yarn"]).unwrap_err().contains("多个"));
        for text in [
            "broken",
            r#"{"scripts":{"start":42}}"#,
            r#"{"packageManager":false}"#,
        ] {
            assert!(parse_node(text, Path::new("package.json")).is_err());
        }
        let package =
            parse_node(r#"{"scripts":{"start":"  "}}"#, Path::new("package.json")).unwrap();
        assert!(node_targets(package, &BTreeSet::new(), Path::new("package.json")).is_err());
    }

    #[test]
    fn generated_names_are_stable_safe_and_do_not_merge_punctuation() {
        let names = [
            "dev:api",
            "dev_3aapi",
            "a/b",
            "a_2fb",
            "启动",
            "-bad",
            "a b",
            "a_20b",
        ];
        let encoded: BTreeSet<_> = names
            .iter()
            .map(|n| entry_name("node", n).unwrap())
            .collect();
        assert_eq!(encoded.len(), names.len());
        assert!(encoded.iter().all(|n| ok_name(n)));
        assert_eq!(entry_name("node", "dev:api").unwrap(), "node-dev_3aapi");
        assert!(entry_name("node", &"x".repeat(100)).is_err());
        assert!(entry_name("node", "\0").is_err());
    }

    #[test]
    fn python_standard_and_poetry_scripts_merge_and_preserve_callables() {
        let text = "[project.scripts]\nworker = 'probe:main'\n[tool.poetry.scripts]\nworker = 'probe:main'\n'api:start' = {reference = 'app.cli:Server.run', type = 'console'}";
        let scripts = python_scripts(text, Path::new("pyproject.toml")).unwrap();
        let targets = python_targets(scripts, ".venv/bin/python").unwrap();
        assert_eq!(targets.len(), 2);
        let api = spec(&targets[0]);
        assert_eq!(api.program, ".venv/bin/python");
        assert_eq!(&api.args[..2], ["-u", "-c"]);
        assert!(api.args[2].contains("Server.run") && api.args[2].contains("app.cli"));
        assert!(api.args[2].contains("sys.argv[0] = \"api:start\""));
    }

    #[test]
    fn python_bad_references_types_and_conflicting_declarations_fail() {
        for text in [
            "[project.scripts]\nworker = 42",
            "[project.scripts]\nworker = 'app.main'",
            "[project.scripts]\nworker = 'app:main()'",
            "[project.scripts]\nworker = 'app:main [extra]'",
            "[project.scripts]\nworker = 'app:main'\n[tool.poetry.scripts]\nworker = 'other:main'",
            "[tool.poetry.scripts]\nworker = {reference = 'worker.py', type = 'file'}",
        ] {
            let error = python_scripts(text, Path::new("pyproject.toml")).unwrap_err();
            assert!(error.contains("pyproject.toml"), "{error}");
        }
        assert!(
            python_scripts("[project]\nname = 'library'", Path::new("pyproject.toml"))
                .unwrap()
                .is_empty()
        );
    }

    fn jar(manifest: Option<&str>) -> Cursor<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(vec![]));
        if let Some(manifest) = manifest {
            zip.start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
        }
        zip.finish().unwrap()
    }

    #[test]
    fn java_requires_main_class_in_main_section_and_reads_continuations() {
        let path = Path::new("app.jar");
        assert!(
            jar_runnable(
                jar(Some(
                    "Manifest-Version: 1.0\r\nMain-Class: example.\r\n Main\r\n\r\n"
                )),
                path
            )
            .unwrap()
        );
        assert!(jar_runnable(jar(Some("main-class: Example\n\n")), path).unwrap());
        for manifest in [
            None,
            Some("Manifest-Version: 1.0\n\n"),
            Some("Manifest-Version: 1.0\n\nName: named-section\nMain-Class: Wrong\n"),
        ] {
            assert!(!jar_runnable(jar(manifest), path).unwrap());
        }
    }

    #[test]
    fn java_corrupt_archives_and_invalid_manifests_return_errors() {
        let path = Path::new("app.jar");
        assert!(
            jar_runnable(Cursor::new(b"broken"), path)
                .unwrap_err()
                .contains("app.jar")
        );
        for text in [
            " broken continuation",
            "Main-Class=Broken",
            "Main-Class: A\nMain-Class: B",
        ] {
            assert!(jar_runnable(jar(Some(text)), path).is_err());
        }
        assert!(
            jar_runnable(jar(Some(&"x".repeat(65537))), path)
                .unwrap_err()
                .contains("64 KiB")
        );
    }

    #[test]
    fn go_source_ignores_comments_literals_methods_and_libraries() {
        assert!(go_main("package main\nfunc /* comment */ main() {}").unwrap());
        for text in [
            "package library\nfunc main() {}",
            "// package main\n// func main() {}",
            "/* package main; func main() {} */\npackage library",
            "package main\nvar sample = `func main() {}`",
            "package main\nvar sample = \"func main() {}\"",
            "package main\nfunc (r Receiver) main() {}",
            "package main\nfunc main(args string) {}",
        ] {
            assert!(!go_main(text).unwrap(), "{text}");
        }
        assert!(go_main("package main /* incomplete").is_err());
        assert!(go_main("package main `incomplete").is_err());
    }

    #[test]
    fn go_module_requires_one_nonempty_module_declaration() {
        let path = Path::new("go.mod");
        assert!(
            check_go_module("// module ignored\nmodule example.test/app\ngo 1.20", path).is_ok()
        );
        for text in [
            "go 1.20",
            "module",
            "module a\nmodule b",
            "module a b",
            "module \"\"",
            "module \"incomplete",
        ] {
            assert!(check_go_module(text, path).unwrap_err().contains("go.mod"));
        }
    }

    #[tokio::test]
    async fn mixed_project_discovers_all_languages_and_excludes_nonentries() {
        let targets = load_targets(&fixture()).await.unwrap();
        assert!(matches!(targets, ProjectTargets::Discovered(_)));
        assert!(targets.is_configured());
        assert_eq!(
            targets.names(),
            [
                "go-cmd_2fapi",
                "go-main",
                "java-app.jar",
                "node-dev_3aapi",
                "node-start",
                "python-worker"
            ]
        );
        assert_eq!(spec(targets.target("go-main").unwrap()).args, ["run", "."]);
        assert_eq!(
            spec(targets.target("go-cmd_2fapi").unwrap()).args,
            ["run", "./cmd/api"]
        );
        let java = spec(targets.target("java-app.jar").unwrap());
        assert_eq!(java.program, "java");
        assert_eq!(java.args[0], "-jar");
        assert_eq!(
            fixture().join(&java.args[1]).canonicalize().unwrap(),
            fixture().join("app.jar").canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn cargo_with_node_build_tooling_keeps_cargo_service_rules() {
        let dir = fixture().parent().unwrap().join("cargo-tooling");
        let targets = load_targets(&dir).await.unwrap();
        assert!(matches!(targets, ProjectTargets::Cargo(_)));
        assert!(!targets.is_configured());
        assert_eq!(targets.names(), ["worker"]);
    }

    #[tokio::test]
    async fn java_jar_path_is_relative_to_project_even_with_relative_scan_root() {
        let dir = Path::new("tests/fixtures/launch");
        let targets = load_targets(dir).await.unwrap();
        let java = spec(targets.target("java-app.jar").unwrap());
        assert!(
            dir.join(&java.args[1]).is_file(),
            "JAR 必须能从启动工作目录解析：{}",
            java.args[1]
        );
        assert_eq!(java.args, ["-jar", "app.jar"]);
    }

    #[tokio::test]
    async fn discovered_commands_keep_one_literal_extra_after_tool_arguments() {
        let targets = load_targets(&fixture()).await.unwrap();
        let extra = "  --port 8080 --label \"two words\"\nnext\t ";
        for name in targets.names() {
            let mut target = spec(targets.target(&name).unwrap()).clone();
            let original = target.args.clone();
            target.program = std::env::current_exe().unwrap().display().to_string();
            let target = Target::Command(target);
            let command = target.prepare(&fixture(), extra, None).unwrap();
            assert_eq!(&command.args[..original.len()], original);
            assert_eq!(command.args.len(), original.len() + 1);
            assert_eq!(command.args.last().unwrap(), extra);
            let argv = systemd_run_argv("panel-fixture-probe.service", &command);
            assert_eq!(argv.last().unwrap(), extra);
            assert_eq!(
                target.prepare(&fixture(), " \t\n", None).unwrap().args,
                original
            );
        }
    }

    #[tokio::test]
    async fn missing_venv_is_distinct_from_present_broken_venv() {
        assert_eq!(
            python_interpreter(&fixture()).await.unwrap(),
            if cfg!(windows) { "python" } else { "python3" }
        );
        let broken = fixture().parent().unwrap().join("launch-broken-venv");
        let error = python_interpreter(&broken).await.unwrap_err();
        assert!(error.contains("缺少 Python 解释器"), "{error}");
    }

    #[tokio::test]
    async fn invalid_automatic_manifest_reports_error_instead_of_empty_targets() {
        let invalid = fixture().parent().unwrap().join("launch-invalid");
        let error = load_targets(&invalid).await.unwrap_err();
        assert!(
            error.contains("package.json") && error.contains("解析"),
            "{error}"
        );
    }

    fn run_probe(command: crate::launch::PreparedCommand, extra: &str) -> serde_json::Value {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .envs(&command.env)
            .current_dir(&command.working_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result: serde_json::Value = stdout
            .lines()
            .find_map(|line| serde_json::from_str(line).ok())
            .unwrap();
        assert_eq!(result["args"], serde_json::json!([extra]));
        assert_eq!(
            Path::new(result["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            fixture().canonicalize().unwrap()
        );
        result
    }

    #[tokio::test]
    #[ignore = "需要本机 Python 运行时"]
    async fn real_python_declared_callable_receives_literal_argument() {
        let targets = load_targets(&fixture()).await.unwrap();
        let extra = "  --port 8080 --label \"two words\"\nnext\t ";
        let mut command = targets
            .target("python-worker")
            .unwrap()
            .prepare(&fixture(), extra, None)
            .unwrap();
        // 探针只读取固定输入，不在测试项目中生成字节码缓存。
        command
            .env
            .insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
        let result = run_probe(command, extra);
        assert_eq!(result["entry"], "worker");
    }

    #[tokio::test]
    #[ignore = "需要本机 Node/npm；Windows 还需 PANEL_TEST_POSIX_SHELL"]
    async fn real_npm_script_receives_literal_argument() {
        let targets = load_targets(&fixture()).await.unwrap();
        let target = spec(targets.target("node-start").unwrap()).clone();
        #[cfg(windows)]
        let target = {
            let mut target = target;
            // Linux 执行 npm；Windows 用同一份 npm CLI 绕过 .cmd 启动器验证透传。
            let node = std::env::split_paths(&std::env::var("PATH").unwrap())
                .map(|p| p.join("node.exe"))
                .find(|p| p.is_file())
                .expect("需要 node.exe");
            let cli = node
                .parent()
                .unwrap()
                .join("node_modules/npm/bin/npm-cli.js");
            assert!(cli.is_file(), "需要 npm CLI：{}", cli.display());
            target.program = node.display().to_string();
            target.args.insert(0, cli.display().to_string());
            // npm 默认的 cmd.exe 不保留换行，使用 POSIX shell 验证 Linux 的脚本转义分支。
            let shell = std::env::var("PANEL_TEST_POSIX_SHELL")
                .expect("Windows 验证需指定 PANEL_TEST_POSIX_SHELL，例如 Git 的 sh.exe");
            assert!(Path::new(&shell).is_file(), "POSIX shell 不存在：{shell}");
            target.env.insert("npm_config_script_shell".into(), shell);
            target
        };
        let extra = "  --port 8080 --label \"two words\"\nnext\t ";
        run_probe(
            Target::Command(target)
                .prepare(&fixture(), extra, None)
                .unwrap(),
            extra,
        );
    }
}
