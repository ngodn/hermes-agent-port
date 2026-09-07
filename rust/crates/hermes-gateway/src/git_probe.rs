//! Noninteractive Git probes for prompt construction.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Copy the caller's environment and replace config injection with the same
/// inert Git settings used by hermes_cli/_subprocess_compat.py. Askpass and
/// SSH-agent variables remain available, matching the reference.
pub fn noninteractive_env(mut env: BTreeMap<OsString, OsString>) -> BTreeMap<OsString, OsString> {
    env.retain(|key, _| {
        let key = key.to_string_lossy();
        key != "GIT_CONFIG_PARAMETERS"
            && key != "GIT_CONFIG_COUNT"
            && !key.starts_with("GIT_CONFIG_KEY_")
            && !key.starts_with("GIT_CONFIG_VALUE_")
    });
    let null = if cfg!(windows) { "nul" } else { "/dev/null" };
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "Never"),
        ("GIT_CONFIG_GLOBAL", null),
        ("GIT_CONFIG_SYSTEM", null),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("GIT_EDITOR", "true"),
    ] {
        env.insert(key.into(), value.into());
    }
    let overrides = [
        ("credential.helper", ""),
        ("core.askPass", ""),
        ("core.fsmonitor", "false"),
        ("core.untrackedCache", "false"),
        ("core.hooksPath", null),
        ("core.pager", "cat"),
        ("core.editor", "true"),
        ("sequence.editor", "true"),
        ("diff.external", ""),
    ];
    env.insert(
        "GIT_CONFIG_COUNT".into(),
        overrides.len().to_string().into(),
    );
    for (index, (key, value)) in overrides.into_iter().enumerate() {
        env.insert(format!("GIT_CONFIG_KEY_{index}").into(), key.into());
        env.insert(format!("GIT_CONFIG_VALUE_{index}").into(), value.into());
    }
    env
}

async fn run(mut command: tokio::process::Command, timeout: Duration) -> String {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let Ok(mut child) = command.spawn() else {
        return String::new();
    };
    let pid = child.id();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let operation = async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok((status, _, _))) => {
            if status.success() {
                String::from_utf8_lossy(&out)
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_owned()
            } else {
                String::new()
            }
        }
        _ => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // This group was created by this invocation, never inherited.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            if let Some(pid) = pid {
                let mut kill = tokio::process::Command::new("taskkill");
                kill.args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .creation_flags(0x08000000);
                let _ = tokio::time::timeout(Duration::from_secs(2), kill.status()).await;
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(
                    child.wait(),
                    stdout.read_to_end(&mut out),
                    stderr.read_to_end(&mut err)
                )
            })
            .await;
            String::new()
        }
    }
}

/// Probe arguments remain separate argv elements. Session construction passes
/// its own environment snapshot rather than changing process-global config.
pub async fn git(
    cwd: &std::path::Path,
    args: &[&str],
    env: BTreeMap<OsString, OsString>,
) -> String {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_clear()
        .envs(noninteractive_env(env));
    run(command, Duration::from_millis(2500)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_pipe_cannot_hold_probe_open() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 30 & printf partial; exit 0"]);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run(command, Duration::from_millis(50)),
        )
        .await;
        assert_eq!(result.unwrap(), "");
    }

    #[tokio::test]
    async fn git_probe_ignores_injected_config_and_reports_real_repo_status() {
        let root = std::env::temp_dir().join(format!("hermes-git-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join("new.txt"), "new").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = root.join(".git/probe-monitor");
            std::fs::write(&script, "#!/bin/sh\ntouch .git/monitor-ran\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["config", "core.fsmonitor"])
                .arg(&script)
                .status()
                .unwrap()
                .success());
        }
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        env.insert("GIT_CONFIG_COUNT".into(), "1".into());
        env.insert("GIT_CONFIG_KEY_0".into(), "core.fsmonitor".into());
        env.insert("GIT_CONFIG_VALUE_0".into(), "must-not-execute".into());
        let status = git(&root, &["status", "--porcelain=2"], env).await;
        assert_eq!(status, "? new.txt");
        assert_eq!(
            git(
                &root,
                &["status", "--porcelain=2"],
                std::env::vars_os().collect()
            )
            .await,
            "? new.txt"
        );
        assert!(!root.join(".git/monitor-ran").exists());
        assert_eq!(
            git(&root, &["not-a-git-command"], std::env::vars_os().collect()).await,
            ""
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
