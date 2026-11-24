// 启停的结果和停止证明共用一条链路，不依靠错误文本猜测 unit 是否存在。
use std::format;

use crate::launch::{PreparedCommand, systemd_run_argv};
use crate::systemd::{Raw, command_message, fail_reason, run, show_checked};

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Unconfirmed(String),
    Failed(String),
}

impl Outcome {
    pub fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Failed(a), Self::Failed(b)) => Self::Failed(format!("{a}；{b}")),
            (failure @ Self::Failed(_), _) | (_, failure @ Self::Failed(_)) => failure,
            (Self::Unconfirmed(a), Self::Unconfirmed(b)) => Self::Unconfirmed(format!("{a}；{b}")),
            (pending @ Self::Unconfirmed(_), _) | (_, pending @ Self::Unconfirmed(_)) => pending,
            _ => Self::Complete,
        }
    }

    pub fn note(&self) -> String {
        match self {
            Self::Complete => String::new(),
            Self::Unconfirmed(message) => format!(" 状态未确认：{message}"),
            Self::Failed(message) => format!(" 失败：{message}"),
        }
    }
}

pub trait Services {
    async fn show(&self, unit: &str) -> Result<Raw, String>;
    async fn invoke(&self, program: &str, args: &[&str]) -> Result<(), String>;
    async fn pause(&self);
    async fn journal(&self, unit: &str) -> String;
}

pub struct SystemServices;

impl Services for SystemServices {
    async fn show(&self, unit: &str) -> Result<Raw, String> { show_checked(unit).await }

    async fn invoke(&self, program: &str, args: &[&str]) -> Result<(), String> {
        match run(program, args).await {
            Ok((true, ..)) => Ok(()),
            Ok((false, stdout, stderr)) => Err(command_message(&stdout, &stderr)),
            Err(e) => Err(format!("执行 {program} 失败：{e}")),
        }
    }

    async fn pause(&self) { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }

    async fn journal(&self, unit: &str) -> String {
        match run("journalctl", &["-u", unit, "-n", "12", "--no-pager", "-o", "short-iso"]).await {
            Ok((_, stdout, stderr)) => format!("{stdout}{stderr}").trim().into(),
            Err(e) => format!("读取日志失败：{e}"),
        }
    }
}

fn stopped(raw: &Raw) -> bool {
    raw.load == "not-found" || (raw.pid == 0 && (raw.active == "inactive" || raw.active == "failed"))
}

pub async fn stop(services: &impl Services, unit: &str) -> Outcome {
    let raw = match services.show(unit).await {
        Ok(raw) => raw,
        Err(error) => return Outcome::Unconfirmed(error),
    };
    if raw.load == "not-found" { return Outcome::Complete; }
    if let Err(error) = services.invoke("systemctl", &["stop", "--", unit]).await {
        // unit 在查询后被移除是唯一可忽略的失败；权限、超时等错误必须保留。
        if services.show(unit).await.is_ok_and(|raw| raw.load == "not-found") {
            return Outcome::Complete;
        }
        return Outcome::Failed(format!("停止 {unit} 失败：{error}"));
    }
    let mut last = Outcome::Unconfirmed(format!("无法确认 {unit} 已停止"));
    for _ in 0..20 {
        match services.show(unit).await {
            Ok(raw) if stopped(&raw) => return Outcome::Complete,
            Ok(raw) => last = Outcome::Failed(format!("停止 {unit} 后仍为 {}，PID {}", raw.active, raw.pid)),
            Err(error) => last = Outcome::Unconfirmed(error),
        }
        services.pause().await;
    }
    last
}

pub async fn settle(services: &impl Services, unit: &str) -> Outcome {
    let mut message = format!("{unit} 的启动状态未确认");
    for _ in 0..8 {
        services.pause().await;
        match services.show(unit).await {
            Ok(raw) if raw.active == "active" => return Outcome::Complete,
            Ok(raw) if raw.active == "failed" => {
                return Outcome::Failed(format!("{}；{}", fail_reason(&raw).unwrap_or_else(|| "启动失败".into()), services.journal(unit).await));
            }
            Ok(raw) if raw.load == "not-found" || raw.active == "inactive" || raw.active == "deactivating" => {
                return Outcome::Failed(format!("{unit} 启动后退出，未留下运行进程"));
            }
            Ok(raw) => message = format!("{unit} 当前为 {}，启动状态未确认", raw.active),
            Err(error) => message = error,
        }
    }
    Outcome::Unconfirmed(message)
}

pub struct LaunchResult {
    pub created: bool,
    pub outcome: Outcome,
}

pub async fn launch(services: &impl Services, unit: &str, command: &PreparedCommand) -> LaunchResult {
    let outcome = stop(services, unit).await;
    if outcome != Outcome::Complete { return LaunchResult { created: false, outcome }; }
    if let Err(error) = services.invoke("systemctl", &["reset-failed", "--", unit]).await
        && !services.show(unit).await.is_ok_and(|raw| raw.load == "not-found") {
            return LaunchResult { created: false, outcome: Outcome::Failed(format!("重置 {unit} 失败：{error}")) };
    }
    let argv = systemd_run_argv(unit, command);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    if let Err(error) = services.invoke("systemd-run", &refs).await {
        return LaunchResult { created: false, outcome: Outcome::Failed(error) };
    }
    LaunchResult { created: true, outcome: settle(services, unit).await }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use std::{matches, vec};

    struct Fake {
        states: Mutex<VecDeque<Result<Raw, String>>>,
        last: Mutex<Result<Raw, String>>,
        invocations: Mutex<Vec<String>>,
        results: Mutex<VecDeque<Result<(), String>>>,
    }

    fn raw(active: &str, pid: u64) -> Raw {
        Raw { load: if active == "missing" { "not-found" } else { "loaded" }.into(),
            active: if active == "missing" { "inactive" } else { active }.into(), pid, ..Raw::default() }
    }

    impl Fake {
        fn new(states: Vec<Result<Raw, String>>, results: Vec<Result<(), String>>) -> Self {
            Self { states: Mutex::new(states.into()), last: Mutex::new(Err("状态不可读".into())),
                invocations: Mutex::new(vec![]), results: Mutex::new(results.into()) }
        }
        fn called(&self) -> Vec<String> { self.invocations.lock().unwrap().clone() }
    }

    impl Services for Fake {
        async fn show(&self, _: &str) -> Result<Raw, String> {
            if let Some(next) = self.states.lock().unwrap().pop_front() {
                *self.last.lock().unwrap() = next;
            }
            self.last.lock().unwrap().clone()
        }
        async fn invoke(&self, program: &str, args: &[&str]) -> Result<(), String> {
            self.invocations.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }
        async fn pause(&self) { tokio::task::yield_now().await; }
        async fn journal(&self, _: &str) -> String { "应用日志".into() }
    }

    fn command() -> PreparedCommand {
        PreparedCommand { program: "/usr/bin/probe".into(), args: vec![],
            working_dir: std::path::PathBuf::from("/"), env: BTreeMap::new() }
    }

    #[tokio::test]
    async fn stop_failure_prevents_reset_and_new_process_creation() {
        let fake = Fake::new(vec![Ok(raw("active", 7))], vec![Err("Access denied".into())]);
        let result = launch(&fake, "probe.service", &command()).await;
        assert!(!result.created);
        assert_eq!(result.outcome, Outcome::Failed("停止 probe.service 失败：Access denied".into()));
        assert_eq!(fake.called(), ["systemctl stop -- probe.service"]);
    }

    #[tokio::test]
    async fn successful_stop_command_still_requires_exit_proof() {
        let fake = Fake::new(vec![Ok(raw("active", 7))], vec![]);
        let result = launch(&fake, "probe.service", &command()).await;
        assert!(!result.created);
        assert!(matches!(result.outcome, Outcome::Failed(_)));
        assert_eq!(fake.called(), ["systemctl stop -- probe.service"]);
    }

    #[tokio::test]
    async fn unreadable_stop_state_never_creates_another_process() {
        let fake = Fake::new(vec![Err("systemd 无响应".into())], vec![]);
        let result = launch(&fake, "probe.service", &command()).await;
        assert!(!result.created);
        assert!(matches!(result.outcome, Outcome::Unconfirmed(_)));
        assert!(fake.called().is_empty());
    }

    #[tokio::test]
    async fn absent_unit_is_benign_but_error_text_alone_is_not() {
        let absent = Fake::new(vec![Ok(raw("missing", 0))], vec![]);
        assert_eq!(stop(&absent, "probe.service").await, Outcome::Complete);
        assert!(absent.called().is_empty());
        let denied = Fake::new(vec![Ok(raw("active", 7))], vec![Err("not loaded: Access denied".into())]);
        assert!(matches!(stop(&denied, "probe.service").await, Outcome::Failed(_)));
    }

    #[tokio::test]
    async fn confirmed_exit_precedes_reset_creation_and_state_check() {
        let fake = Fake::new(vec![Ok(raw("active", 7)), Ok(raw("deactivating", 7)),
            Ok(raw("inactive", 0)), Ok(raw("active", 8))], vec![]);
        let result = launch(&fake, "probe.service", &command()).await;
        assert!(result.created);
        assert_eq!(result.outcome, Outcome::Complete);
        let called = fake.called();
        assert_eq!(called[0], "systemctl stop -- probe.service");
        assert_eq!(called[1], "systemctl reset-failed -- probe.service");
        assert!(called[2].starts_with("systemd-run --unit=probe.service"));
    }

    #[tokio::test]
    async fn unreadable_and_activating_start_states_remain_unconfirmed() {
        for state in [Err("无状态".into()), Ok(raw("activating", 0)), Ok(raw("reloading", 9))] {
            let fake = Fake::new(vec![state], vec![]);
            assert!(matches!(settle(&fake, "probe.service").await, Outcome::Unconfirmed(_)));
        }
        let fake = Fake::new(vec![Ok(raw("failed", 0))], vec![]);
        assert!(matches!(settle(&fake, "probe.service").await, Outcome::Failed(message) if message.contains("应用日志")));
    }

    #[test]
    fn aggregation_keeps_failure_and_unconfirmed_results_distinct() {
        assert_eq!(Outcome::Complete.merge(Outcome::Unconfirmed("等待".into())), Outcome::Unconfirmed("等待".into()));
        assert_eq!(Outcome::Unconfirmed("等待".into()).merge(Outcome::Failed("失败".into())), Outcome::Failed("失败".into()));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "需要 Linux systemd、五种运行时以及 PANEL_TEST_PANEL_BIN"]
    async fn real_systemd_language_targets_keep_literal_arguments() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let extra = "  --label \"two words\" 'single'\nnext\t $literal ";
        for (index, (project, target, marker)) in [
            ("node-service", "node-start", "NODE_SERVICE_OK"),
            ("python-files", "python-main.py", "PYTHON_FILE_OK"),
            ("go-probe", "go-main", "GO_LAUNCH_OK"),
            ("java-maven", "java-maven", "JAVA_LAUNCH_OK"),
            ("java-gradle", "java-gradle", "JAVA_LAUNCH_OK"),
            ("cargo-feature-probe", "rust-probe", "RUST_LAUNCH_OK"),
            ("cargo-feature-probe", "single", "RUST_LAUNCH_OK"),
            ("cargo-feature-probe", "multiple", "RUST_LAUNCH_OK"),
        ].into_iter().enumerate() {
            let dir = root.join("tests/fixtures").join(project);
            let targets = crate::launch::load_targets(&dir).await.unwrap();
            let selected = targets.target(target).unwrap();
            let mut command = selected.prepare(&dir,extra,None).unwrap();
            if matches!(selected, crate::launch::Target::JavaBuild(_)) {
                command.program = std::env::var("PANEL_TEST_PANEL_BIN").unwrap();
            }
            command.env.insert("PANEL_EXPECT_ARGUMENT".into(), extra.into());
            command.env.insert("PANEL_PROBE_SLEEP".into(), "1".into());
            command.env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
            let unit = format!("panel-validation-{}-{stamp}-{index}.service",std::process::id());
            struct Cleanup(String);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::process::Command::new("systemctl").args(["stop", "--", &self.0]).status();
                    let _ = std::process::Command::new("systemctl").args(["reset-failed", "--", &self.0]).status();
                }
            }
            let _cleanup = Cleanup(unit.clone());
            let result = launch(&SystemServices,&unit,&command).await;
            assert!(result.created && result.outcome == Outcome::Complete, "{target}: {:?}",result.outcome);
            let mut ready = false;
            for _ in 0..120 {
                if SystemServices.journal(&unit).await.contains(marker) { ready = true; break; }
                if let Ok(raw) = show_checked(&unit).await {
                    assert_eq!(raw.active,"active","{target}: {}",SystemServices.journal(&unit).await);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            assert!(ready,"{target}: {}",SystemServices.journal(&unit).await);
            let raw = show_checked(&unit).await.unwrap();
            assert!(std::fs::read_to_string(format!("/proc/{}/cgroup",raw.pid)).unwrap().contains(&unit));
            assert_eq!(stop(&SystemServices,&unit).await,Outcome::Complete);
            assert!(!std::path::Path::new(&format!("/proc/{}",raw.pid)).exists());
            println!("Linux 实跑通过：{project}/{target}；完整参数、日志、cgroup、停止；{unit}");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "需要 Linux systemd 与创建 transient unit 的权限"]
    async fn real_systemd_creates_tracks_logs_and_stops_only_probe_unit() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let unit = format!("panel-validation-{}-{stamp}.service", std::process::id());
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::process::Command::new("systemctl").args(["stop", "--", &self.0]).status();
                let _ = std::process::Command::new("systemctl").args(["reset-failed", "--", &self.0]).status();
            }
        }
        let _cleanup = Cleanup(unit.clone());
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut command = PreparedCommand {
            program: std::env::current_exe().unwrap().display().to_string(),
            args: vec!["--exact".into(), "lifecycle::tests::service_probe".into(), "--nocapture".into(), "--test-threads=1".into()],
            working_dir: dir.clone(),
            env: BTreeMap::from([("PANEL_SYSTEMD_PROBE".into(), unit.clone()),
                ("PANEL_SYSTEMD_CWD".into(), dir.display().to_string()),
                ("PANEL_LITERAL".into(), "引号 \" 空格\t".into())]),
        };
        let result = launch(&SystemServices, &unit, &command).await;
        assert!(result.created && result.outcome == Outcome::Complete, "{:?}", result.outcome);
        let state = show_checked(&unit).await.unwrap();
        assert!(state.pid > 0);
        let cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", state.pid)).unwrap();
        assert!(cgroup.contains(&unit), "{cgroup}");
        let mut ready = false;
        for _ in 0..30 {
            if SystemServices.journal(&unit).await.contains("PANEL_SYSTEMD_READY") { ready = true; break; }
            SystemServices.pause().await;
        }
        assert!(ready, "探针未完成环境与参数检查");
        command.env.insert("PANEL_SYSTEMD_SECOND".into(), "1".into());
        let restarted = launch(&SystemServices, &unit, &command).await;
        assert!(restarted.created && restarted.outcome == Outcome::Complete);
        let next = show_checked(&unit).await.unwrap();
        assert_ne!(next.pid, state.pid);
        assert!(!std::path::Path::new(&format!("/proc/{}", state.pid)).exists());
        let mut second_ready = false;
        for _ in 0..30 {
            if SystemServices.journal(&unit).await.contains("PANEL_SYSTEMD_SECOND_READY") { second_ready = true; break; }
            SystemServices.pause().await;
        }
        assert!(second_ready, "重启后的探针未完成检查");
        assert_eq!(stop(&SystemServices, &unit).await, Outcome::Complete);
        command.program = "/panel-validation-executable-does-not-exist".into();
        let failed = launch(&SystemServices, &unit, &command).await;
        assert!(matches!(failed.outcome, Outcome::Failed(_)), "{:?}", failed.outcome);
        assert!(matches!(launch(&SystemServices,"panel-validation/invalid.service",&command).await.outcome, Outcome::Unconfirmed(_)));
        println!("已验证 transient unit、cgroup、日志、参数、环境、目录、停止、重启、错误与未知状态：{unit}");
    }

    #[test]
    fn service_probe() {
        let Ok(unit) = std::env::var("PANEL_SYSTEMD_PROBE") else { return; };
        assert_eq!(std::env::args().next_back().unwrap(), "--test-threads=1");
        assert_eq!(std::env::var("PANEL_LITERAL").unwrap(), "引号 \" 空格\t");
        assert_eq!(std::env::current_dir().unwrap(), std::path::PathBuf::from(std::env::var("PANEL_SYSTEMD_CWD").unwrap()));
        assert!(std::fs::read_to_string("/proc/self/cgroup").unwrap().contains(&unit));
        println!("PANEL_SYSTEMD_READY");
        if std::env::var("PANEL_SYSTEMD_SECOND").as_deref() == Ok("1") { println!("PANEL_SYSTEMD_SECOND_READY"); }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
