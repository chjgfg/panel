// 只补齐运行工具必需的用户主目录，不复制面板的其他环境变量。
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::format;
use std::path::Path;

pub fn ensure_home(env: &mut BTreeMap<String, String>) -> Result<(), String> {
    let home = resolve_home(
        env.get("HOME")
            .map(OsString::from)
            .or_else(|| std::env::var_os("HOME")),
        account_home,
    )?;
    env.insert("HOME".into(), home);
    Ok(())
}

fn resolve_home(
    supplied: Option<OsString>,
    account: impl FnOnce() -> Result<OsString, String>,
) -> Result<String, String> {
    let value = match supplied {
        Some(value) => value,
        None => account()?,
    };
    let home = value
        .into_string()
        .map_err(|_| "启动环境 HOME 不是有效 UTF-8 路径".to_string())?;
    if home.is_empty() || home.contains(['\0', '\n', '\r']) || !Path::new(&home).is_absolute() {
        return Err("启动环境 HOME 必须是有效的绝对目录路径".into());
    }
    let metadata =
        std::fs::metadata(&home).map_err(|e| format!("读取启动环境 HOME 目录 {home} 失败：{e}"))?;
    if !metadata.is_dir() {
        return Err(format!("启动环境 HOME 不是目录：{home}"));
    }
    Ok(home)
}

#[cfg(unix)]
fn account_home() -> Result<OsString, String> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;
    use std::vec;

    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // 缓冲区与账户结构在调用及读取期间保持有效，成功且指针非空后才读取字段。
        let code = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if code != 0 {
            return Err(format!(
                "读取当前用户主目录失败：{}",
                std::io::Error::from_raw_os_error(code)
            ));
        }
        if result.is_null() {
            return Err("读取当前用户主目录失败：账户不存在，请显式配置 HOME".into());
        }
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return Err("读取当前用户主目录失败：账户缺少主目录，请显式配置 HOME".into());
        }
        return Ok(OsString::from_vec(
            unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes().to_vec(),
        ));
    }
}

#[cfg(windows)]
fn account_home() -> Result<OsString, String> {
    std::env::var_os("USERPROFILE")
        .ok_or_else(|| "读取当前用户主目录失败：缺少 USERPROFILE，请显式配置 HOME".into())
}

#[cfg(not(any(unix, windows)))]
fn account_home() -> Result<OsString, String> {
    Err("当前平台无法读取用户主目录，请显式配置 HOME".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> String {
        env!("CARGO_MANIFEST_DIR").into()
    }

    #[test]
    fn supplied_home_is_preserved_and_never_falls_back() {
        assert_eq!(
            resolve_home(Some(root().into()), || panic!("已有 HOME 不得查账户")).unwrap(),
            root()
        );
        for value in ["", ".", "invalid\npath", "missing"] {
            assert!(resolve_home(Some(value.into()), || panic!("无效 HOME 不得静默回退")).is_err());
        }
        let file = Path::new(&root()).join("Cargo.toml");
        assert!(
            resolve_home(Some(file.into_os_string()), || panic!("不得回退"))
                .unwrap_err()
                .contains("不是目录")
        );
        let missing = Path::new(&root()).join("panel-home-does-not-exist");
        assert!(
            resolve_home(Some(missing.into_os_string()), || panic!("不得回退"))
                .unwrap_err()
                .contains("读取")
        );
    }

    #[test]
    fn missing_home_uses_account_and_reports_lookup_failure() {
        assert_eq!(resolve_home(None, || Ok(root().into())).unwrap(), root());
        assert_eq!(
            resolve_home(None, || Err("账户读取失败".into())).unwrap_err(),
            "账户读取失败"
        );
        assert!(resolve_home(None, || Ok("".into())).is_err());
    }

    #[test]
    fn explicit_and_default_home_reach_real_preparation_and_systemd_argv() {
        use crate::launch::{CommandTarget, Target, systemd_run_argv};
        use std::vec;
        let program = std::env::current_exe().unwrap().display().to_string();
        for supplied in [None, Some(root())] {
            let env = supplied
                .clone()
                .map(|home| BTreeMap::from([("HOME".into(), home)]))
                .unwrap_or_default();
            let target = Target::Command(CommandTarget {
                name: "probe".into(),
                program: program.clone(),
                args: vec![],
                working_dir: ".".into(),
                env,
            });
            let command = target.prepare(Path::new(&root()), "", None).unwrap();
            assert_eq!(
                command.env["HOME"],
                supplied.unwrap_or_else(
                    || resolve_home(std::env::var_os("HOME"), account_home).unwrap()
                )
            );
            assert!(
                systemd_run_argv("probe.service", &command)
                    .contains(&format!("--setenv=HOME={}", command.env["HOME"]))
            );
        }
        let cargo = Target::Cargo {
            name: "probe".into(),
            features: vec![],
        }
        .prepare(Path::new(&root()), "", Some(&program))
        .unwrap();
        assert!(!cargo.env["HOME"].is_empty());
        let invalid = Target::Command(CommandTarget {
            name: "probe".into(),
            program,
            args: vec![],
            working_dir: ".".into(),
            env: BTreeMap::from([("HOME".into(), "".into())]),
        });
        assert!(
            invalid
                .prepare(Path::new(&root()), "", None)
                .unwrap_err()
                .contains("HOME")
        );
    }

    #[cfg(unix)]
    #[test]
    fn account_home_is_used_when_parent_environment_is_absent() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_env::tests::home_child_probe",
                "--nocapture",
            ])
            .env_clear()
            .env("PANEL_HOME_PROBE", "1")
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

    #[cfg(unix)]
    #[test]
    fn home_child_probe() {
        if std::env::var("PANEL_HOME_PROBE").as_deref() != Ok("1") {
            return;
        }
        assert!(std::env::var_os("HOME").is_none());
        let mut env = BTreeMap::new();
        ensure_home(&mut env).unwrap();
        assert_eq!(env["HOME"], resolve_home(None, account_home).unwrap());
    }
}
