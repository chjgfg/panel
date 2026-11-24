// 项目启动入口：显式命令配置优先，没有配置才自动发现各语言的入口。
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::{format, matches, vec};

use serde::{Deserialize, Serialize};

use crate::discover::{find_cargo, ok_name};

pub const CONFIG_FILE: &str = "panel-run.toml";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    targets: Vec<CommandTarget>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandTarget {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "project_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn project_dir() -> String {
    ".".into()
}

#[derive(Clone, Debug)]
pub enum Target {
    Cargo { name: String, features: Vec<String> },
    Command(CommandTarget),
    JavaBuild(crate::java_launch::JavaTarget),
}

impl Target {
    pub fn name(&self) -> &str {
        match self {
            Self::Cargo { name, .. } => name,
            Self::Command(target) => &target.name,
            Self::JavaBuild(target) => &target.name,
        }
    }

    pub fn prepare(
        &self,
        dir: &Path,
        extra_args: &str,
        cargo: Option<&str>,
    ) -> Result<PreparedCommand, String> {
        if extra_args.contains('\0') {
            return Err("程序参数不能包含 NUL 字符".into());
        }
        match self {
            Self::JavaBuild(target) => target.prepare(dir, extra_args),
            Self::Cargo { name, features } => {
                let cargo = find_cargo(cargo).ok_or_else(|| {
                    "找不到 cargo，请在 panel.toml 的 cargo 项指定可执行文件的绝对路径".to_string()
                })?;
                let working_dir = checked_dir(dir)?;
                let program = resolve_program(&cargo, &working_dir, &default_path())?;
                Ok(PreparedCommand::cargo(
                    &program,
                    &working_dir,
                    name,
                    features,
                    extra_args,
                ))
            }
            Self::Command(target) => {
                let working_dir = checked_dir(&dir.join(&target.working_dir))?;
                let search_path = target.env.get("PATH").cloned().unwrap_or_else(default_path);
                let program = resolve_program(&target.program, &working_dir, &search_path)?;
                let mut env = target.env.clone();
                if !env.contains_key("PATH") {
                    env.insert("PATH".into(), path_with_program(&program, &search_path)?);
                }
                let mut args = target.args.clone();
                append_extra(&mut args, extra_args);
                Ok(PreparedCommand {
                    program,
                    args,
                    working_dir,
                    env,
                })
            }
        }
    }
}

/// 保留配置及自动发现的来源，避免把失败当成自动回退。
#[derive(Debug)]
pub enum ProjectTargets {
    Configured(Vec<Target>),
    Cargo(Vec<Target>),
    Discovered(Vec<Target>),
    Unconfigured,
}

impl ProjectTargets {
    pub fn preview(&self, dir: &Path, name: &str, extra: &str, cargo: Option<&str>) -> Result<CommandPreview, String> {
        let target = self.target(name)?;
        let command = target.prepare(dir, extra, cargo)?;
        let source = match self {
            Self::Configured(_) => CONFIG_FILE.into(),
            Self::Cargo(_) => "Cargo.toml".into(),
            _ => match target {
                Target::Cargo { .. } => "Cargo.toml".into(),
                Target::JavaBuild(spec) => spec.source.clone(),
                Target::Command(_) if name.starts_with("node-") => "package.json".into(),
                Target::Command(spec) if name.starts_with("java-") => spec.args.get(1).cloned().unwrap_or_default(),
                Target::Command(spec) if name.starts_with("python-") => {
                    if spec.args.get(1).is_some_and(|p| p.ends_with(".py")) { spec.args[1].clone() } else { "pyproject.toml".into() }
                },
                _ => "go.mod / Go 源码".into(),
            },
        };
        let features = match target { Target::Cargo { features, .. } => features.clone(), _ => vec![] };
        let (program, args, build) = if let Target::JavaBuild(_) = target {
            let plan: crate::java_launch::RunPlan = serde_json::from_str(&command.args[1]).map_err(|e| e.to_string())?;
            let artifact = match plan.artifact {
                crate::java_launch::Artifact::Jar => "构建后选择唯一带 Main-Class 的 JAR，使用 java -jar 启动".into(),
                crate::java_launch::Artifact::Distribution { main_class } => format!("构建后使用 distribution/lib/* 的 classpath 启动 {main_class}"),
            };
            (plan.java, command.args[2..].to_vec(), Some(BuildPreview { program: plan.builder, args: plan.build_args, artifact }))
        } else { (command.program, command.args, None) };
        Ok(CommandPreview { bin: name.into(), source, features, program,
            args, working_dir: command.working_dir, build })
    }
    pub fn names(&self) -> Vec<String> {
        self.entries()
            .iter()
            .map(|target| target.name().to_string())
            .collect()
    }

    pub fn target(&self, name: &str) -> Result<&Target, String> {
        self.entries()
            .iter()
            .find(|target| target.name() == name)
            .ok_or_else(|| format!("这个项目里没有启动入口「{name}」"))
    }

    pub fn is_configured(&self) -> bool {
        // 跨语言入口也优先展示；纯 Cargo 项目保留原来的手写服务选择规则。
        matches!(self, Self::Configured(_) | Self::Discovered(_))
    }

    fn entries(&self) -> &[Target] {
        match self {
            Self::Configured(targets) | Self::Cargo(targets) | Self::Discovered(targets) => targets,
            Self::Unconfigured => &[],
        }
    }
}

// 预览复用实际启动准备结果，仅返回命令信息，不回传环境变量值。
#[derive(Debug, Serialize)]
pub struct CommandPreview {
    pub bin: String,
    pub source: String,
    pub features: Vec<String>,
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub build: Option<BuildPreview>,
}

#[derive(Debug, Serialize)]
pub struct BuildPreview { pub program: String, pub args: Vec<String>, pub artifact: String }

pub(crate) enum FileSource {
    Present(String),
    Missing,
}

pub(crate) async fn read_config(path: &Path) -> Result<FileSource, String> {
    // 先查目录项，断开的符号链接也算配置存在，读取失败不能自动回退。
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => tokio::fs::read_to_string(path)
            .await
            .map(FileSource::Present)
            .map_err(|e| format!("读取 {} 失败：{e}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(FileSource::Missing),
        Err(e) => Err(format!("检查 {} 失败：{e}", path.display())),
    }
}

pub async fn load_targets(dir: &Path) -> Result<ProjectTargets, String> {
    let path = dir.join(CONFIG_FILE);
    let source = read_config(&path).await?;
    targets_from_source(dir, &path, source).await
}

async fn targets_from_source(
    dir: &Path,
    path: &Path,
    source: FileSource,
) -> Result<ProjectTargets, String> {
    match source {
        FileSource::Present(text) => {
            let config = parse_config(&text, path)?;
            Ok(ProjectTargets::Configured(
                config.targets.into_iter().map(Target::Command).collect(),
            ))
        }
        FileSource::Missing => crate::auto_launch::discover_targets(dir).await,
    }
}

fn parse_config(text: &str, path: &Path) -> Result<ProjectConfig, String> {
    let config: ProjectConfig =
        toml::from_str(text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    let bad = |message: String| format!("配置 {} 有问题：{message}", path.display());
    if config.targets.is_empty() {
        return Err(bad("至少配置一个 [[targets]] 启动入口".into()));
    }
    let mut names = BTreeSet::new();
    for target in &config.targets {
        if !ok_name(&target.name) || !names.insert(&target.name) {
            return Err(bad(format!("启动入口名称非法或重复：{}", target.name)));
        }
        if target.program.trim().is_empty() || target.program.contains('\0') {
            return Err(bad(format!(
                "「{}」的 program 必须是可执行程序",
                target.name
            )));
        }
        if target.working_dir.trim().is_empty()
            || target.working_dir.contains('\0')
            || target.args.iter().any(|arg| arg.contains('\0'))
        {
            return Err(bad(format!("「{}」的工作目录或参数非法", target.name)));
        }
        for (key, value) in &target.env {
            let mut chars = key.chars();
            let valid = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid || value.contains(['\0', '\n', '\r']) {
                return Err(bad(format!("「{}」的环境变量非法：{key}", target.name)));
            }
        }
    }
    Ok(config)
}

#[derive(Debug, Serialize)]
pub struct PreparedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl PreparedCommand {
    pub fn cargo(
        cargo: &str,
        dir: &Path,
        bin: &str,
        features: &[String],
        extra_args: &str,
    ) -> Self {
        let mut args = vec!["run".into()];
        if !features.is_empty() {
            args.extend(["--features".into(), features.join(",")]);
        }
        args.extend(["--bin".into(), bin.into()]);
        if !extra_args.trim().is_empty() {
            args.push("--".into());
            args.push(extra_args.to_string());
        }
        let parent = Path::new(cargo)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            program: cargo.into(),
            args,
            working_dir: dir.to_path_buf(),
            env: BTreeMap::from([(
                "PATH".into(),
                format!("{parent}:/usr/local/bin:/usr/bin:/bin"),
            )]),
        }
    }
}

fn append_extra(args: &mut Vec<String>, extra: &str) {
    // 参数框保持旧语义：非空字符串整个追加一次，不拆分，不裁掉首尾空白。
    if !extra.trim().is_empty() {
        args.push(extra.to_string());
    }
}

fn checked_dir(dir: &Path) -> Result<PathBuf, String> {
    let absolute = dir
        .canonicalize()
        .map_err(|e| format!("读取工作目录 {} 失败：{e}", dir.display()))?;
    if !absolute.is_dir() {
        return Err(format!("工作目录不是目录：{}", dir.display()));
    }
    Ok(absolute)
}

fn default_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
}

fn path_with_program(program: &str, search_path: &str) -> Result<String, String> {
    let parent = Path::new(program)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    std::env::join_paths(
        std::iter::once(parent.to_path_buf()).chain(std::env::split_paths(search_path)),
    )
    .map(|path| path.to_string_lossy().into_owned())
    .map_err(|e| format!("构造启动环境 PATH 失败：{e}"))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_program(program: &str, dir: &Path, search_path: &str) -> Result<String, String> {
    let path = Path::new(program);
    let explicit = path.is_absolute() || path.components().count() > 1;
    let candidates: Vec<PathBuf> = if explicit {
        vec![dir.join(path)]
    } else {
        std::env::split_paths(search_path)
            .map(|entry| dir.join(entry).join(path))
            .collect()
    };
    for candidate in candidates {
        if executable(&candidate) {
            return Ok(candidate.display().to_string());
        }
        #[cfg(windows)]
        if !explicit && path.extension().is_none() {
            let candidate = candidate.with_extension("exe");
            if executable(&candidate) {
                return Ok(candidate.display().to_string());
            }
        }
    }
    Err(format!(
        "找不到可执行程序或没有执行权限：{program}（工作目录 {}）",
        dir.display()
    ))
}

pub fn systemd_run_argv(unit: &str, command: &PreparedCommand) -> Vec<String> {
    let mut argv = vec![
        format!("--unit={unit}"),
        format!("--working-directory={}", command.working_dir.display()),
    ];
    argv.extend(
        command
            .env
            .iter()
            .map(|(key, value)| format!("--setenv={key}={value}")),
    );
    argv.push(command.program.clone());
    argv.extend(command.args.clone());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    fn example() -> ProjectConfig {
        parse_config(
            include_str!("../panel-run.toml.example"),
            Path::new(CONFIG_FILE),
        )
        .unwrap()
    }

    fn command_target() -> CommandTarget {
        CommandTarget {
            name: "probe".into(),
            program: std::env::current_exe().unwrap().display().to_string(),
            args: vec![],
            working_dir: ".".into(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn preview_uses_the_launch_command_and_does_not_expose_environment_values() {
        let mut spec = command_target();
        spec.env.insert("SECRET".into(), "hidden-value".into());
        spec.args = vec!["first argument".into()];
        let extra = "  a \"quote\"\nnext\t ";
        let targets = ProjectTargets::Configured(vec![Target::Command(spec)]);
        let command = targets.target("probe").unwrap().prepare(root(), extra, None).unwrap();
        let preview = targets.preview(root(), "probe", extra, None).unwrap();
        assert_eq!(preview.program, command.program);
        assert_eq!(preview.args, command.args);
        assert_eq!(preview.working_dir, command.working_dir);
        assert_eq!(preview.source, CONFIG_FILE);
        assert!(!serde_json::to_string(&preview).unwrap().contains("hidden-value"));
        assert!(targets.preview(root(), "missing", extra, None).is_err());
    }

    #[test]
    fn example_config_roundtrips_all_five_command_types() {
        let config = example();
        assert_eq!(config.targets.len(), 5);
        assert_eq!(
            config
                .targets
                .iter()
                .map(|t| t.program.as_str())
                .collect::<Vec<_>>(),
            ["java", "npm", "python3", "cargo", "./YOUR_GO_BINARY"]
        );
        let exported = toml::to_string(&config).unwrap();
        assert_eq!(
            parse_config(&exported, Path::new(CONFIG_FILE)).unwrap(),
            config
        );
    }

    #[tokio::test]
    async fn explicit_config_wins_over_cargo_discovery() {
        let targets = targets_from_source(
            root(),
            &root().join(CONFIG_FILE),
            FileSource::Present(include_str!("../panel-run.toml.example").into()),
        )
        .await
        .unwrap();
        assert!(matches!(targets, ProjectTargets::Configured(_)));
        assert_eq!(
            targets.names(),
            [
                "java-api",
                "node-api",
                "python-worker",
                "rust-worker",
                "go-api"
            ]
        );
        assert!(targets.target("panel").is_err());
    }

    #[tokio::test]
    async fn explicit_config_wins_over_all_automatic_languages_and_broken_manifests() {
        for project in ["launch", "launch-invalid"] {
            let dir = root().join("tests/fixtures").join(project);
            let targets = targets_from_source(
                &dir,
                &dir.join(CONFIG_FILE),
                FileSource::Present("[[targets]]\nname = 'chosen'\nprogram = 'python3'".into()),
            )
            .await
            .unwrap();
            assert!(matches!(targets, ProjectTargets::Configured(_)));
            assert_eq!(targets.names(), ["chosen"]);
        }
    }

    #[tokio::test]
    async fn missing_config_keeps_cargo_and_unconfigured_sources_distinct() {
        assert!(matches!(
            load_targets(root()).await.unwrap(),
            ProjectTargets::Cargo(_)
        ));
        let plain = load_targets(&root().join("src")).await.unwrap();
        assert!(matches!(plain, ProjectTargets::Unconfigured));
        assert!(plain.names().is_empty());
        assert!(plain.target("anything").is_err());
    }

    #[tokio::test]
    async fn broken_explicit_config_never_falls_back_to_valid_cargo() {
        let error = targets_from_source(
            root(),
            &root().join(CONFIG_FILE),
            FileSource::Present("targets = 'broken'".into()),
        )
        .await
        .unwrap_err();
        assert!(
            error.contains(CONFIG_FILE) && error.contains("解析"),
            "{error}"
        );
        let error = match read_config(&root().join("src")).await {
            Err(error) => error,
            Ok(_) => panic!("配置路径是目录时必须报错"),
        };
        assert!(error.contains("读取"), "{error}");
    }

    #[test]
    fn config_rejects_bad_types_names_fields_and_environment() {
        for text in [
            "targets = []",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\nargs = 'a b'",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\narguments = []",
            "[[targets]]\nname = '-probe'\nprogram = 'node'",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\n[[targets]]\nname = 'probe'\nprogram = 'python3'",
            "[[targets]]\nname = 'probe'\nprogram = '   '",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\nworking_dir = ''",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\nargs = [\"\\u0000\"]",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\n[targets.env]\n'BAD=KEY' = 'x'",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\n[targets.env]\n'1BAD' = 'x'",
            "[[targets]]\nname = 'probe'\nprogram = 'node'\n[targets.env]\nOK = \"a\\nb\"",
        ] {
            let error = parse_config(text, Path::new(CONFIG_FILE)).unwrap_err();
            assert!(error.contains(CONFIG_FILE), "{error}");
        }
    }

    #[test]
    fn every_command_preserves_array_arguments_and_one_unsplit_extra() {
        let extra = "  --port 8080  --label \"two words\"\nnext\t ";
        let expected = [
            vec!["-jar", "YOUR_APP.jar"],
            vec!["run", "start", "--"],
            vec!["-u", "-m", "YOUR_MODULE"],
            vec!["run", "--bin", "YOUR_BIN", "--"],
            vec![],
        ];
        for (mut spec, args) in example().targets.into_iter().zip(expected) {
            // 只替换运行时路径，五种示例的参数及环境仍走同一条生产准备链路。
            spec.program = std::env::current_exe().unwrap().display().to_string();
            let target = Target::Command(spec);
            let command = target
                .prepare(root(), extra, Some("unavailable-cargo"))
                .unwrap();
            let want: Vec<String> = args
                .into_iter()
                .map(String::from)
                .chain([extra.to_string()])
                .collect();
            assert_eq!(command.args, want);
            let argv = systemd_run_argv("panel-example-probe.service", &command);
            assert_eq!(&argv[argv.len() - want.len()..], want);
        }
    }

    #[test]
    fn command_defaults_and_empty_array_values_are_preserved() {
        let config = parse_config(
            "[[targets]]\nname = 'probe'\nprogram = 'node'",
            Path::new(CONFIG_FILE),
        )
        .unwrap();
        assert_eq!(config.targets[0].working_dir, ".");
        assert!(config.targets[0].args.is_empty() && config.targets[0].env.is_empty());
        let mut spec = command_target();
        spec.args = vec!["".into(), "two words".into(), "\"quoted\"".into()];
        let target = Target::Command(spec);
        assert_eq!(
            target.prepare(root(), " \t\n", None).unwrap().args,
            ["", "two words", "\"quoted\""]
        );
    }

    #[test]
    fn runtime_and_working_directory_errors_are_explicit() {
        assert!(
            Target::Command(command_target())
                .prepare(root(), "\0", None)
                .is_err()
        );
        let mut spec = command_target();
        spec.program = "panel-runtime-that-does-not-exist".into();
        spec.env.insert(
            "PATH".into(),
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .display()
                .to_string(),
        );
        let error = Target::Command(spec).prepare(root(), "", None).unwrap_err();
        assert!(error.contains("找不到可执行程序"), "{error}");
        let mut spec = command_target();
        spec.working_dir = "Cargo.toml".into();
        let error = Target::Command(spec).prepare(root(), "", None).unwrap_err();
        assert!(error.contains("工作目录不是目录"), "{error}");
        let target = Target::Cargo {
            name: "panel".into(),
            features: vec![],
        };
        let error = target
            .prepare(root(), "", Some("panel-cargo-that-does-not-exist"))
            .unwrap_err();
        assert!(error.contains("找不到 cargo"), "{error}");
    }

    #[test]
    fn relative_program_and_target_path_are_used() {
        let executable = std::env::current_exe().unwrap();
        let parent = executable.parent().unwrap();
        let filename = executable.file_name().unwrap().to_str().unwrap();
        let mut spec = command_target();
        spec.program = filename.into();
        spec.env.insert("PATH".into(), parent.display().to_string());
        let command = Target::Command(spec).prepare(root(), "", None).unwrap();
        assert_eq!(Path::new(&command.program), executable);
        assert_eq!(command.env["PATH"], parent.display().to_string());
        let mut spec = command_target();
        spec.program = Path::new(".").join(filename).display().to_string();
        spec.working_dir = parent.display().to_string();
        let command = Target::Command(spec).prepare(root(), "", None).unwrap();
        assert_eq!(
            Path::new(&command.program).canonicalize().unwrap(),
            executable.canonicalize().unwrap()
        );
    }

    #[test]
    fn local_child_receives_prepared_environment_directory_and_arguments() {
        let mut spec = command_target();
        spec.working_dir = "src".into();
        spec.args = vec![
            "--exact".into(),
            "launch::tests::runtime_child_probe".into(),
            "--nocapture".into(),
        ];
        spec.env = BTreeMap::from([
            ("PANEL_LAUNCH_PROBE".into(), "1".into()),
            (
                "PANEL_EXPECTED_CWD".into(),
                root()
                    .join("src")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
            ),
            (
                "PANEL_LITERAL_ENV".into(),
                "value with spaces \"quotes\" $literal".into(),
            ),
        ]);
        let command = Target::Command(spec)
            .prepare(root(), "--test-threads=1", None)
            .unwrap();
        let argv = systemd_run_argv("panel-example-probe.service", &command);
        assert!(argv.contains(
            &"--setenv=PANEL_LITERAL_ENV=value with spaces \"quotes\" $literal".to_string()
        ));
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
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn runtime_child_probe() {
        if std::env::var("PANEL_LAUNCH_PROBE").as_deref() != Ok("1") {
            return;
        }
        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            Path::new(&std::env::var("PANEL_EXPECTED_CWD").unwrap())
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            std::env::var("PANEL_LITERAL_ENV").unwrap(),
            "value with spaces \"quotes\" $literal"
        );
        assert_eq!(std::env::args().next_back().unwrap(), "--test-threads=1");
    }
}
