// Java 源码入口先构建再直接执行产物，避免构建工具重新拆分程序参数。
use std::collections::BTreeMap;
use std::path::Path;
use std::{format, matches, vec};
use serde::{Deserialize, Serialize};
use crate::launch::{CommandTarget, FileSource, PreparedCommand, Target, read_config};

pub const RUN_MODE: &str = "--panel-java-build";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Artifact {
    Jar,
    Distribution { main_class: String },
}

#[derive(Clone, Debug)]
pub struct JavaTarget {
    pub name: String,
    pub source: String,
    pub tool: String,
    pub build_args: Vec<String>,
    pub artifact: Artifact,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    pub builder: String,
    pub build_args: Vec<String>,
    pub java: String,
    pub artifact: Artifact,
}

impl JavaTarget {
    pub fn prepare(&self, dir: &Path, extra: &str) -> Result<PreparedCommand, String> {
        let runtime = |program: &str| Target::Command(CommandTarget {
            name: self.name.clone(), program: program.into(), args: vec![],
            working_dir: ".".into(), env: BTreeMap::new(),
        }).prepare(dir, "", None);
        let builder = runtime(&self.tool)?;
        let java = runtime("java")?;
        let plan = RunPlan { builder: builder.program, build_args: self.build_args.clone(),
            java: java.program.clone(), artifact: self.artifact.clone() };
        let mut command = runtime(&std::env::current_exe().map_err(|e| format!("读取 panel 路径失败：{e}"))?.display().to_string())?;
        command.args = vec![RUN_MODE.into(), serde_json::to_string(&plan).map_err(|e| e.to_string())?];
        if !extra.trim().is_empty() { command.args.push(extra.into()); }
        // 构建工具可能通过 PATH 寻找 java，保留两个运行时的实际目录。
        let path = builder.env.get("PATH").cloned().unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(Path::new(&java.program).parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&path))).map_err(|e| format!("构造 Java PATH 失败：{e}"))?;
        command.env.insert("PATH".into(), joined.to_string_lossy().into_owned());
        Ok(command)
    }
}

async fn wrapper(dir: &Path, relative: &str, fallback: &str) -> Result<String, String> {
    match tokio::fs::symlink_metadata(dir.join(relative)).await {
        Ok(_) => Ok(format!("./{relative}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(fallback.into()),
        Err(e) => Err(format!("检查 {relative} 失败：{e}")),
    }
}

pub async fn discover(dir: &Path) -> Result<Vec<Target>, String> {
    let mut targets = vec![];
    let pom = dir.join("pom.xml");
    if let FileSource::Present(text) = read_config(&pom).await? && maven_boot(&text, &pom)? {
            targets.push(Target::JavaBuild(JavaTarget { name: "java-maven".into(), source: "pom.xml".into(),
                tool: wrapper(dir, "mvnw", "mvn").await?, build_args: vec!["-B".into(), "-ntp".into(), "-DskipTests".into(), "package".into()], artifact: Artifact::Jar }));
    }
    let mut gradle = None;
    for file in ["build.gradle", "build.gradle.kts"] {
        if let FileSource::Present(text) = read_config(&dir.join(file)).await? {
            if gradle.is_some() { return Err("同时存在 build.gradle 和 build.gradle.kts，请用 panel-run.toml 明确入口".into()); }
            gradle = Some((file, text));
        }
    }
    if let Some((file, text)) = gradle && let Some(artifact) = gradle_artifact(&text, &dir.join(file))? {
            let task = if matches!(artifact, Artifact::Jar) { "bootJar" } else { "installDist" };
            targets.push(Target::JavaBuild(JavaTarget { name: "java-gradle".into(), source: file.into(),
                tool: wrapper(dir, "gradlew", "gradle").await?, build_args: vec!["--no-daemon".into(), "--console=plain".into(), task.into()], artifact }));
    }
    Ok(targets)
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, tag: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == tag)
}

fn maven_boot(text: &str, path: &Path) -> Result<bool, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    let root = doc.root_element();
    if !root.has_tag_name("project") { return Err(format!("{} 缺少 project 根节点", path.display())); }
    if child(root, "packaging").and_then(|n| n.text()).is_some_and(|t| t.trim() == "pom") { return Ok(false); }
    let Some(plugins) = child(root, "build").and_then(|n| child(n, "plugins")) else { return Ok(false); };
    Ok(plugins.children().filter(|n| n.is_element() && n.tag_name().name() == "plugin").any(|plugin| {
        child(plugin, "groupId").and_then(|n| n.text()).is_some_and(|t| t.trim() == "org.springframework.boot")
            && child(plugin, "artifactId").and_then(|n| n.text()).is_some_and(|t| t.trim() == "spring-boot-maven-plugin")
    }))
}

#[derive(Debug)]
struct Token { text: String, literal: bool }

fn gradle_tokens(text: &str, path: &Path) -> Result<Vec<Token>, String> {
    let bad = || format!("解析 {} 失败：未闭合的注释、字符串或括号", path.display());
    let mut chars = text.chars().peekable();
    let mut tokens = vec![];
    let mut brackets = vec![];
    while let Some(c) = chars.next() {
        if c.is_whitespace() { continue; }
        if c == '/' && chars.peek() == Some(&'/') {
            chars.next(); for ch in chars.by_ref() { if ch == '\n' { break; } } continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); let mut closed = false;
            while let Some(ch) = chars.next() { if ch == '*' && chars.peek() == Some(&'/') { chars.next(); closed = true; break; } }
            if !closed { return Err(bad()); } continue;
        }
        if c == '\'' || c == '"' {
            let mut value = String::new(); let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == c { closed = true; break; }
                if ch == '\\' { value.push(chars.next().ok_or_else(bad)?); } else { value.push(ch); }
            }
            if !closed { return Err(bad()); }
            tokens.push(Token { text: value, literal: true }); continue;
        }
        if "{([".contains(c) { brackets.push(c); }
        if "})]".contains(c) {
            let expected = match c { '}' => '{', ')' => '(', _ => '[' };
            if brackets.pop() != Some(expected) { return Err(bad()); }
        }
        let mut word = c.to_string();
        if c.is_alphanumeric() || c == '_' {
            while chars.peek().is_some_and(|c| c.is_alphanumeric() || *c == '_') { word.push(chars.next().unwrap()); }
        }
        tokens.push(Token { text: word, literal: false });
    }
    if !brackets.is_empty() { return Err(bad()); }
    Ok(tokens)
}

fn block<'a>(tokens: &'a [Token], name: &str) -> Option<&'a [Token]> {
    // 只识别根项目的声明，subprojects/buildscript 中的同名配置不能当成根入口。
    let mut nesting = 0;
    let mut start = None;
    for (i, token) in tokens.iter().enumerate() {
        if !token.literal {
            if nesting == 0 && token.text == name && tokens.get(i+1).is_some_and(|t| !t.literal && t.text == "{") { start = Some(i+2); break; }
            if token.text == "{" { nesting += 1; }
            if token.text == "}" { nesting -= 1; }
        }
    }
    let start = start?;
    let mut depth = 1;
    for i in start..tokens.len() {
        if !tokens[i].literal {
            if tokens[i].text == "{" { depth += 1; }
            if tokens[i].text == "}" { depth -= 1; }
        }
        if depth == 0 { return Some(&tokens[start..i]); }
    }
    None
}

fn plugin_disabled(tokens: &[Token], mut end: usize) -> bool {
    if tokens.get(end).is_some_and(|t| t.text == ".") { end += 1; }
    if tokens.get(end).is_none_or(|t| t.text != "apply") { return false; }
    end += 1;
    if tokens.get(end).is_some_and(|t| t.text == "(") { end += 1; }
    // 动态 apply 也不假定已经应用，只有明确 true 才作为根入口。
    tokens.get(end).is_none_or(|t| t.literal || t.text != "true")
}

fn gradle_artifact(text: &str, path: &Path) -> Result<Option<Artifact>, String> {
    let tokens = gradle_tokens(text, path)?;
    let Some(plugins) = block(&tokens, "plugins") else { return Ok(None); };
    let mut ids = vec![];
    let mut nested = 0;
    for (i, token) in plugins.iter().enumerate() {
        if !token.literal && matches!(token.text.as_str(), "(" | "{") { nested += 1; continue; }
        if !token.literal && matches!(token.text.as_str(), ")" | "}") { nested -= 1; continue; }
        if nested != 0 { continue; }
        if !token.literal && token.text == "application"
            && (i == 0 || plugins[i-1].text != ".")
            && !plugin_disabled(plugins,i+1) { ids.push("application"); }
        if !token.literal && token.text == "id" {
            let mut end = i + 1;
            if plugins.get(end).is_some_and(|t| t.text == "(") { end += 1; }
            let Some(id) = plugins.get(end).filter(|t| t.literal) else { continue; };
            end += 1;
            if plugins.get(end).is_some_and(|t| t.text == ")") { end += 1; }
            if plugins.get(end).is_some_and(|t| t.text == "version") { end += 2; }
            if plugin_disabled(plugins,end) { continue; }
            ids.push(&id.text);
        }
    }
    if ids.contains(&"org.springframework.boot") { return Ok(Some(Artifact::Jar)); }
    if !ids.contains(&"application") { return Ok(None); }
    let application = block(&tokens, "application").unwrap_or(&tokens);
    let mut main = None;
    for (i, token) in application.iter().enumerate() {
        if token.literal || !matches!(token.text.as_str(), "mainClass" | "mainClassName") { continue; }
        let rest = &application[i+1..];
        let value = if rest.first().is_some_and(|t| t.text == "=") { rest.get(1) }
            else if rest.len() >= 4 && rest[0].text == "." && rest[1].text == "set" && rest[2].text == "(" { rest.get(3) } else { None };
        let end = if rest.first().is_some_and(|t| t.text == "=") { 2 } else { 5 };
        let continuation = rest.get(end).is_some_and(|t| matches!(t.text.as_str(), "+" | "."));
        let Some(value) = value.filter(|v| v.literal && valid_main(&v.text) && !continuation) else {
            return Err(format!("{} 的 application mainClass 必须是静态类名，请用 panel-run.toml 配置动态入口", path.display()));
        };
        if main.as_ref().is_some_and(|old| old != &value.text) { return Err(format!("{} 的 mainClass 声明冲突", path.display())); }
        main = Some(value.text.clone());
    }
    let main_class = main.ok_or_else(|| format!("{} 的 application 未声明静态 mainClass，请用 panel-run.toml 配置", path.display()))?;
    Ok(Some(Artifact::Distribution { main_class }))
}

fn valid_main(name: &str) -> bool {
    name.split('.').all(|part| {
        let mut chars = part.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    })
}

pub async fn application_command(plan: &RunPlan, dir: &Path, extra: &[String]) -> Result<Vec<String>, String> {
    let mut args = match &plan.artifact {
        Artifact::Jar => {
            let candidates = crate::auto_launch::java_targets(dir).await?;
            let built: Vec<_> = candidates.into_iter().filter_map(|target| match target {
                Target::Command(spec) if spec.args.get(1).is_some_and(|p| p.starts_with("target/") || p.starts_with("build/libs/")) => Some(spec.args),
                _ => None,
            }).collect();
            if built.len() != 1 { return Err(format!("构建后找到 {} 个可执行 JAR，需要唯一 Main-Class 产物；请清理构建产物或用 panel-run.toml 指定", built.len())); }
            built.into_iter().next().unwrap()
        }
        Artifact::Distribution { main_class } => {
            if !valid_main(main_class) { return Err("Gradle mainClass 非法".into()); }
            let paths = crate::auto_launch::entries(&dir.join("build/install")).await?;
            let mut libs = vec![];
            for path in paths {
                let lib = path.join("lib");
                if crate::auto_launch::entries(&lib).await?.iter().any(|p| p.extension().is_some_and(|e| e == "jar")) { libs.push(lib); }
            }
            if libs.len() != 1 { return Err(format!("installDist 后找到 {} 个运行目录，需要唯一 distribution；请用 panel-run.toml 指定", libs.len())); }
            vec!["-cp".into(), libs[0].join("*").display().to_string(), main_class.clone()]
        }
    };
    args.extend_from_slice(extra);
    Ok(args)
}

pub async fn execute(plan: RunPlan, extra: Vec<String>) -> Result<i32, String> {
    if extra.len() > 1 || extra.iter().any(|s| s.contains('\0')) { return Err("Java 入口只接受一个完整程序参数".into()); }
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    println!("Java 构建：{} {:?}", plan.builder, plan.build_args);
    let status = tokio::process::Command::new(&plan.builder).args(&plan.build_args)
        .stdin(std::process::Stdio::null()).status().await.map_err(|e| format!("执行 Java 构建失败：{e}"))?;
    if !status.success() { return Err(format!("Java 构建失败：{status}")); }
    let args = application_command(&plan, &dir, &extra).await?;
    println!("Java 启动：{} {:?}", plan.java, args);
    let status = tokio::process::Command::new(&plan.java).args(args)
        .stdin(std::process::Stdio::null()).status().await.map_err(|e| format!("执行 Java 应用失败：{e}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[tokio::test]
    async fn namespaced_maven_and_kotlin_gradle_fixtures_are_discovered_without_building() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for project in ["java-maven", "java-gradle"] {
            let targets = discover(&root.join(project)).await.unwrap();
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].name(), project);
        }
    }
    #[test]
    fn maven_requires_an_applied_plugin_and_valid_xml() {
        let p = Path::new("pom.xml");
        let plugin = "<plugin><groupId>org.springframework.boot</groupId><artifactId>spring-boot-maven-plugin</artifactId></plugin>";
        assert!(maven_boot(&format!("<project><build><plugins>{plugin}</plugins></build></project>"), p).unwrap());
        for text in [format!("<project><build><pluginManagement><plugins>{plugin}</plugins></pluginManagement></build></project>"), format!("<project><!-- {plugin} --></project>"), "<project><packaging>pom</packaging></project>".into()] { assert!(!maven_boot(&text,p).unwrap()); }
        assert!(maven_boot("<project>",p).is_err());
    }
    #[test]
    fn gradle_handles_kotlin_groovy_and_rejects_ambiguous_main_classes() {
        let p = Path::new("build.gradle.kts");
        for text in ["plugins { application }; application { mainClass.set(\"example.Main\") }", "plugins { id 'application' }; application { mainClass = 'example.Main' }"] {
            assert_eq!(gradle_artifact(text,p).unwrap(), Some(Artifact::Distribution { main_class: "example.Main".into() }));
        }
        assert_eq!(gradle_artifact("plugins { id(\"org.springframework.boot\") version \"3.5.7\" }",p).unwrap(),Some(Artifact::Jar));
        for text in ["// plugins { application }", "plugins { id 'org.springframework.boot' version '3.5.7' apply false }", "plugins { id 'java-library' }", "subprojects { plugins { application }; application { mainClass='Child' } }", "plugins { alias(libs.plugins.application) }", "plugins { application.apply(false) }", "plugins { id('org.springframework.boot').apply(false) }"] { assert!(gradle_artifact(text,p).unwrap().is_none()); }
        for text in ["plugins { application }", "plugins { application }; application { mainClass = providers.env('MAIN') }", "plugins { application }; application { mainClass='a.Main' + suffix }", "plugins { application }; application { mainClass='a.Main'; mainClass='b.Main' }", "plugins { application"] { assert!(gradle_artifact(text,p).is_err()); }
    }
    #[test]
    fn java_build_plan_roundtrip_preserves_tool_parameters() {
        let plan = RunPlan { builder: "/path with spaces/mvnw".into(), build_args: vec!["-B".into(), "package".into()], java: "/jdk/bin/java".into(), artifact: Artifact::Jar };
        let decoded: RunPlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(decoded.builder, plan.builder);
        assert_eq!(decoded.build_args, plan.build_args);
    }
    #[tokio::test]
    async fn absent_or_multiple_artifacts_are_errors_before_java_launch() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let plan = RunPlan { builder: "unused".into(), build_args: vec![], java: "unused".into(), artifact: Artifact::Jar };
        assert!(application_command(&plan,&root.join("tests/fixtures/launch"),&[]).await.unwrap_err().contains("0 个"));
        assert!(application_command(&plan,&root.join("tests/fixtures/java-ambiguous"),&[]).await.unwrap_err().contains("2 个"));
    }

    #[tokio::test]
    async fn built_jar_arguments_keep_one_complete_extra_after_artifact() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java-single");
        let plan = RunPlan { builder: "unused".into(), build_args: vec![], java: "unused".into(), artifact: Artifact::Jar };
        let extra = "  --label \"two words\" 'single'\nnext\t ";
        assert_eq!(application_command(&plan,&root,&[extra.into()]).await.unwrap(), ["-jar", "build/libs/app.jar", extra]);
    }

    async fn real_build(project: &str, name: &str) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(project);
        let targets = crate::launch::load_targets(&dir).await.unwrap();
        let extra = "  --label \"two words\" 'single'\nnext\t $literal ";
        let mut command = targets.target(name).unwrap().prepare(&dir, extra, None).unwrap();
        // 测试程序没有 main 的 CLI 分支，使用同次构建的真实 panel 二进制验收该分支。
        let binary = std::env::var("PANEL_TEST_PANEL_BIN").expect("请设置同次构建的 PANEL_TEST_PANEL_BIN");
        assert!(Path::new(&binary).is_file());
        command.program = binary;
        let preview = targets.preview(&dir,name,extra,None).unwrap();
        assert_eq!(preview.program, serde_json::from_str::<RunPlan>(&command.args[1]).unwrap().java);
        assert_eq!(preview.args, [extra]);
        assert!(preview.build.is_some());
        let output = tokio::process::Command::new(&command.program).args(&command.args).envs(&command.env)
            .env("PANEL_EXPECT_ARGUMENT", extra).current_dir(&dir).output().await.unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("JAVA_LAUNCH_OK"));
    }

    #[tokio::test]
    #[ignore = "需要 JDK、Maven 和同次构建的 PANEL_TEST_PANEL_BIN"]
    async fn real_maven_build_and_runner_preserve_literal_application_argument() {
        real_build("java-maven", "java-maven").await;
    }

    #[tokio::test]
    #[ignore = "需要 JDK、Gradle 和同次构建的 PANEL_TEST_PANEL_BIN"]
    async fn real_gradle_distribution_and_runner_preserve_literal_application_argument() {
        real_build("java-gradle", "java-gradle").await;
    }
}
