//! Bounded foreground-process execution for native tools.
//!
//! Callers supply the complete environment and render the typed outcome. This
//! module owns process groups, concurrent pipe draining, bounded retention,
//! private overflow spills, timeout cleanup, and reaping.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;

#[derive(Clone, Copy, Debug)]
pub struct OutputBounds {
    pub head_bytes: usize,
    pub tail_bytes: usize,
}

impl OutputBounds {
    fn limit(self) -> usize {
        self.head_bytes.saturating_add(self.tail_bytes).max(1)
    }
}

#[derive(Clone, Debug)]
pub struct SpillConfig {
    pub dir: PathBuf,
    pub max_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct ForegroundCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub timeout: Duration,
    pub bounds: OutputBounds,
    pub spill: Option<SpillConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedStream {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: usize,
    pub spill_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Exited {
    pub code: Option<i32>,
    pub success: bool,
    pub stdout: BoundedStream,
    pub stderr: BoundedStream,
}

#[derive(Clone, Debug)]
pub struct TimedOut {
    pub stdout: BoundedStream,
    pub stderr: BoundedStream,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Exited(Exited),
    SpawnFailed(String),
    TimedOut(TimedOut),
}

struct SpillWriter {
    file: std::fs::File,
    path: PathBuf,
    written: usize,
    cap: usize,
    capped: bool,
}

impl SpillWriter {
    fn write(&mut self, bytes: &[u8]) {
        if self.capped {
            return;
        }
        let budget = self.cap.saturating_sub(self.written);
        let take = budget.min(bytes.len());
        if take > 0 && self.file.write_all(&bytes[..take]).is_ok() {
            self.written = self.written.saturating_add(take);
        }
        if take < bytes.len() {
            let _ = self
                .file
                .write_all(b"\n... [spill capped before command output ended] ...\n");
            self.capped = true;
        }
    }
}

struct Collector {
    bounds: OutputBounds,
    spill_config: Option<SpillConfig>,
    spill_label: &'static str,
    spill: Option<SpillWriter>,
    spill_failed: bool,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl Collector {
    fn new(
        bounds: OutputBounds,
        spill_config: Option<SpillConfig>,
        spill_label: &'static str,
    ) -> Self {
        Self {
            bounds,
            spill_config,
            spill_label,
            spill: None,
            spill_failed: false,
            head: Vec::with_capacity(bounds.head_bytes),
            tail: VecDeque::with_capacity(bounds.tail_bytes),
            total: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.spill.is_none()
            && !self.spill_failed
            && self.total.saturating_add(bytes.len()) > self.bounds.limit()
        {
            match self
                .spill_config
                .as_ref()
                .map(|config| open_spill(config, self.spill_label))
            {
                Some(Ok(mut spill)) => {
                    let backlog: Vec<u8> = self
                        .head
                        .iter()
                        .copied()
                        .chain(self.tail.iter().copied())
                        .collect();
                    spill.write(&backlog);
                    self.spill = Some(spill);
                }
                Some(Err(error)) => {
                    tracing::warn!(target: "foreground_exec", %error, "output spill unavailable");
                    self.spill_failed = true;
                }
                None => self.spill_failed = true,
            }
        }
        if let Some(spill) = &mut self.spill {
            spill.write(bytes);
        }

        self.total = self.total.saturating_add(bytes.len());
        let take = self
            .bounds
            .head_bytes
            .saturating_sub(self.head.len())
            .min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        if self.bounds.tail_bytes == 0 {
            return;
        }
        self.tail.extend(bytes[take..].iter().copied());
        while self.tail.len() > self.bounds.tail_bytes {
            self.tail.pop_front();
        }
    }

    fn finish(mut self) -> BoundedStream {
        let spill_path = self.spill.take().map(|mut spill| {
            let _ = spill.file.flush();
            let _ = spill.file.sync_all();
            spill.path
        });
        let truncated = self.total > self.bounds.limit();
        let raw = if truncated {
            let omitted = self
                .total
                .saturating_sub(self.head.len())
                .saturating_sub(self.tail.len());
            let mut visible = self.head;
            visible.extend_from_slice(
                format!(
                    "\n\n... [OUTPUT TRUNCATED - {omitted} bytes omitted out of {} total] ...\n\n",
                    self.total
                )
                .as_bytes(),
            );
            visible.extend(self.tail);
            visible
        } else {
            let mut visible = self.head;
            visible.extend(self.tail);
            visible
        };
        BoundedStream {
            text: normalize_crlf(&String::from_utf8_lossy(&raw)),
            truncated,
            total_bytes: self.total,
            spill_path,
        }
    }
}

/// Run one foreground command and return only after its direct child has been
/// reaped. Pipe readers retain a bounded head and tail while the command runs.
pub async fn run(command: ForegroundCommand) -> Outcome {
    let mut child_command = tokio::process::Command::new(&command.program);
    child_command
        .args(&command.args)
        .current_dir(&command.cwd)
        .env_clear()
        .envs(&command.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    child_command.process_group(0);
    #[cfg(windows)]
    child_command.creation_flags(0x0800_0000);

    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => return Outcome::SpawnFailed(error.to_string()),
    };
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out = Arc::new(Mutex::new(Collector::new(
        command.bounds,
        command.spill.clone(),
        "stdout",
    )));
    let err = Arc::new(Mutex::new(Collector::new(
        command.bounds,
        command.spill,
        "stderr",
    )));
    let mut out_task = tokio::spawn(capture(stdout, out.clone()));
    let mut err_task = tokio::spawn(capture(stderr, err.clone()));

    match tokio::time::timeout(command.timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout_settled = settle_capture(&mut out_task, Duration::from_millis(300)).await;
            let stderr_settled = settle_capture(&mut err_task, Duration::from_millis(300)).await;
            if !stdout_settled || !stderr_settled {
                kill_process_group(pid);
            }
            Outcome::Exited(Exited {
                code: status.code(),
                success: status.success(),
                stdout: take_collector(out),
                stderr: take_collector(err),
            })
        }
        Ok(Err(error)) => {
            kill_group(&mut child, pid).await;
            let _ = settle_capture(&mut out_task, Duration::from_secs(1)).await;
            let _ = settle_capture(&mut err_task, Duration::from_secs(1)).await;
            Outcome::SpawnFailed(error.to_string())
        }
        Err(_) => {
            kill_group(&mut child, pid).await;
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            let _ = settle_capture(&mut out_task, Duration::from_secs(1)).await;
            let _ = settle_capture(&mut err_task, Duration::from_secs(1)).await;
            Outcome::TimedOut(TimedOut {
                stdout: take_collector(out),
                stderr: take_collector(err),
            })
        }
    }
}

async fn capture<R>(mut stream: R, collector: Arc<Mutex<Collector>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => collector.lock().unwrap().append(&chunk[..read]),
        }
    }
}

async fn settle_capture(task: &mut tokio::task::JoinHandle<()>, wait: Duration) -> bool {
    if tokio::time::timeout(wait, &mut *task).await.is_err() {
        task.abort();
        let _ = task.await;
        false
    } else {
        true
    }
}

fn take_collector(collector: Arc<Mutex<Collector>>) -> BoundedStream {
    Arc::try_unwrap(collector)
        .unwrap_or_else(|_| panic!("capture task retained its collector"))
        .into_inner()
        .unwrap()
        .finish()
}

async fn kill_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    kill_process_group(pid);
    #[cfg(windows)]
    if let Some(pid) = pid {
        let mut kill = tokio::process::Command::new("taskkill");
        kill.args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags(0x0800_0000);
        let _ = tokio::time::timeout(Duration::from_secs(2), kill.status()).await;
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn open_spill(config: &SpillConfig, label: &str) -> std::io::Result<SpillWriter> {
    std::fs::create_dir_all(&config.dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.dir, std::fs::Permissions::from_mode(0o700))?;
    }
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..16 {
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = config.dir.join(format!(
            "out-{}-{stamp}-{sequence}-{label}.log",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                return Ok(SpillWriter {
                    file,
                    path,
                    written: 0,
                    cap: config.max_bytes,
                    capped: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique spill file",
    ))
}

fn normalize_crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn shell(script: &str, timeout: Duration) -> ForegroundCommand {
        let mut env = BTreeMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            env.insert("PATH".into(), path);
        }
        ForegroundCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env,
            timeout,
            bounds: OutputBounds {
                head_bytes: 4096,
                tail_bytes: 4096,
            },
            spill: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hermes-foreground-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn success_and_nonzero_exit_are_typed() {
        let success = run(shell("printf 'hello'", Duration::from_secs(5))).await;
        let failure = run(shell("printf oops 1>&2; exit 3", Duration::from_secs(5))).await;
        match success {
            Outcome::Exited(exit) => {
                assert!(exit.success);
                assert_eq!(exit.code, Some(0));
                assert_eq!(exit.stdout.text, "hello");
            }
            other => panic!("expected exit, got {other:?}"),
        }
        match failure {
            Outcome::Exited(exit) => {
                assert!(!exit.success);
                assert_eq!(exit.code, Some(3));
                assert_eq!(exit.stderr.text, "oops");
            }
            other => panic!("expected exit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn both_streams_are_drained_without_unbounded_retention() {
        let mut command = shell("seq 1 100000; seq 1 100000 1>&2", Duration::from_secs(20));
        command.bounds = OutputBounds {
            head_bytes: 64,
            tail_bytes: 96,
        };
        let outcome = run(command).await;
        let Outcome::Exited(exit) = outcome else {
            panic!("expected exit, got {outcome:?}");
        };
        assert!(exit.stdout.truncated);
        assert!(exit.stderr.truncated);
        assert!(exit.stdout.text.len() < 300);
        assert!(exit.stderr.text.len() < 300);
        assert!(exit.stdout.total_bytes > 500_000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overflow_spills_privately_to_unique_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("spill");
        let mut command = shell("printf full-output", Duration::from_secs(5));
        command.bounds = OutputBounds {
            head_bytes: 2,
            tail_bytes: 2,
        };
        command.spill = Some(SpillConfig {
            dir: dir.clone(),
            max_bytes: 1024,
        });
        let first = run(command.clone()).await;
        let second = run(command).await;
        let paths = [first, second].map(|outcome| match outcome {
            Outcome::Exited(exit) => exit.stdout.spill_path.unwrap(),
            other => panic!("expected exit, got {other:?}"),
        });
        assert_ne!(paths[0], paths[1]);
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "full-output");
        assert_eq!(
            std::fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_preserves_partial_output_and_kills_descendants() {
        let dir = temp_dir("timeout");
        let marker = dir.join("descendant-ran");
        let script = format!(
            "(sleep 1; touch '{}') & printf partial; sleep 30",
            marker.display()
        );
        let outcome = run(shell(&script, Duration::from_millis(100))).await;
        let Outcome::TimedOut(timed) = outcome else {
            panic!("expected timeout, got {outcome:?}");
        };
        assert!(timed.stdout.text.contains("partial"));
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!marker.exists(), "a descendant survived the group kill");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_parent_cannot_leave_pipe_holding_descendant() {
        let dir = temp_dir("detached");
        let marker = dir.join("descendant-ran");
        let script = format!("(sleep 1; touch '{}') & exit 0", marker.display());
        let outcome = run(shell(&script, Duration::from_secs(5))).await;
        let Outcome::Exited(exit) = outcome else {
            panic!("expected exit, got {outcome:?}");
        };
        assert!(exit.success);
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            !marker.exists(),
            "a descendant survived parent exit cleanup"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supplied_environment_is_authoritative() {
        assert!(std::env::var_os("HOME").is_some());
        let mut command = shell(
            "printf '%s:%s' \"$KEPT\" \"${HOME-unset}\"",
            Duration::from_secs(5),
        );
        command.env.insert("KEPT".into(), "profile".into());
        let outcome = run(command).await;
        let Outcome::Exited(exit) = outcome else {
            panic!("expected exit, got {outcome:?}");
        };
        assert_eq!(exit.stdout.text, "profile:unset");
    }

    #[test]
    fn crlf_normalization_and_utf8_cuts_are_safe() {
        let collector = Arc::new(Mutex::new(Collector::new(
            OutputBounds {
                head_bytes: 5,
                tail_bytes: 5,
            },
            None,
            "test",
        )));
        collector
            .lock()
            .unwrap()
            .append("abc\r\n世界abcdefghijklmnopqrstuvwxyz".as_bytes());
        let output = take_collector(collector);
        assert!(output.truncated);
        assert!(std::str::from_utf8(output.text.as_bytes()).is_ok());
        assert!(!output.text.contains("\r\n"));
    }
}
