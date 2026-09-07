//! Bounded local Python toolchain probe used once during prompt construction.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ProbeFacts {
    python3: Option<String>,
    python: Option<String>,
    python3_has_pip: bool,
    pip_python: Option<String>,
    pep668: bool,
    uv: bool,
}

impl ProbeFacts {
    fn render(&self) -> String {
        let mismatch = self.pip_python.as_ref().is_some_and(|pip| {
            self.python3
                .as_ref()
                .is_some_and(|python| !python.starts_with(pip))
        });
        if self.python3.is_some() && self.python3_has_pip && !mismatch && (!self.pep668 || self.uv)
        {
            return String::new();
        }
        let mut bits = Vec::new();
        match &self.python3 {
            Some(version) => bits.push(format!(
                "python3={version}{}",
                if self.python3_has_pip {
                    ""
                } else {
                    " (no pip module)"
                }
            )),
            None => bits.push("python3=missing".into()),
        }
        match (&self.python, &self.python3) {
            (Some(python), python3) if Some(python) != python3.as_ref() => {
                bits.push(format!("python={python}"));
            }
            (None, Some(_)) => bits.push("python=missing (use python3)".into()),
            _ => {}
        }
        match &self.pip_python {
            Some(version) if mismatch => bits.push(format!("pip→python{version} (mismatch)")),
            Some(version) if !self.python3_has_pip => bits.push(format!("pip→python{version}")),
            None if !self.python3_has_pip => bits.push("pip=missing".into()),
            _ => {}
        }
        if self.pep668 {
            bits.push("PEP 668=yes (use venv or uv)".into());
        }
        if self.uv {
            bits.push("uv=installed".into());
        }
        if bits.is_empty() {
            String::new()
        } else {
            format!("Python toolchain: {}.", bits.join(", "))
        }
    }
}

fn executable(name: &str, environment: &BTreeMap<OsString, OsString>) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.components().count() > 1 && direct.is_file() {
        return Some(direct.to_owned());
    }
    environment
        .get(OsStr::new("PATH"))
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn run(
    executable: &Path,
    args: &[&str],
    environment: &BTreeMap<OsString, OsString>,
) -> Option<String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
    let stem = format!("hermes-env-probe-{}-{suffix}", std::process::id());
    let output_path = std::env::temp_dir().join(format!("{stem}.out"));
    let error_path = std::env::temp_dir().join(format!("{stem}.err"));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output_path)
        .ok()?;
    let error = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&error_path)
    {
        Ok(error) => error,
        Err(_) => {
            let _ = std::fs::remove_file(&output_path);
            return None;
        }
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = std::fs::remove_file(&output_path);
            let _ = std::fs::remove_file(&error_path);
            return None;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let mut text = String::new();
    if success {
        let _ =
            std::fs::File::open(&output_path).and_then(|mut file| file.read_to_string(&mut text));
    }
    let _ = std::fs::remove_file(output_path);
    let _ = std::fs::remove_file(error_path);
    success
        .then(|| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn version(name: &str, environment: &BTreeMap<OsString, OsString>) -> Option<String> {
    let executable = executable(name, environment)?;
    run(
        &executable,
        &["-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}')"],
        environment,
    )
}

pub async fn line(environment: &BTreeMap<OsString, OsString>) -> String {
    let environment = environment.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let python3 = version("python3", &environment);
        let python = version("python", &environment);
        let python3_executable = executable("python3", &environment);
        let python3_has_pip = python3_executable.as_ref().is_some_and(|executable| {
            run(executable, &["-m", "pip", "--version"], &environment).is_some()
        });
        let pip_python = executable("pip", &environment)
            .and_then(|pip| run(&pip, &["--version"], &environment))
            .and_then(|output| {
                let (_, tail) = output.rsplit_once("(python ")?;
                tail.strip_suffix(')').map(str::trim).map(str::to_owned)
            });
        let pep668 = python3_executable.as_ref().is_some_and(|executable| {
            run(executable, &["-c", "import os; print('yes' if os.path.exists(os.path.join(os.path.dirname(os.__file__), 'EXTERNALLY-MANAGED')) else 'no')"], &environment).as_deref() == Some("yes")
        });
        ProbeFacts {
            python3,
            python,
            python3_has_pip,
            pip_python,
            pep668,
            uv: executable("uv", &environment).is_some(),
        }
        .render()
    });
    match tokio::time::timeout(Duration::from_secs(10), probe).await {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => {
            tracing::debug!(%error, "Python environment probe worker failed");
            String::new()
        }
        Err(_) => {
            tracing::warn!("Python environment probe timed out; omitting prompt hint");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_only_non_default_toolchain_states() {
        assert!(ProbeFacts {
            python3: Some("3.12.1".into()),
            python: Some("3.12.1".into()),
            python3_has_pip: true,
            ..Default::default()
        }
        .render()
        .is_empty());
        assert_eq!(
            ProbeFacts {
                python3: Some("3.12.1".into()),
                python: None,
                python3_has_pip: false,
                pip_python: Some("3.11".into()),
                pep668: true,
                uv: true,
            }
            .render(),
            "Python toolchain: python3=3.12.1 (no pip module), python=missing (use python3), pip→python3.11 (mismatch), PEP 668=yes (use venv or uv), uv=installed."
        );
    }
}
