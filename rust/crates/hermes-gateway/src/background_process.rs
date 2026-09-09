//! Gateway-owned local background processes for native tools.
//!
//! One registry survives conversation-client eviction. Each operation carries
//! an owner key, so a frozen tool can see only its profile and conversation
//! route. The module owns process groups, bounded merged output, prefix
//! resolution, process retention, waiting, and ordered shutdown.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const MIN_PREFIX_CHARS: usize = 4;
const DEFAULT_RETAINED_CHARS: usize = 200_000;
const MAX_PROCESSES: usize = 64;
const FINISHED_TTL: Duration = Duration::from_secs(30 * 60);
const CAPTURE_SETTLE: Duration = Duration::from_millis(300);
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    profile_home: PathBuf,
    session_key: String,
}

impl Owner {
    pub fn new(profile_home: &Path, session_key: impl Into<String>) -> Self {
        Self {
            profile_home: profile_home.to_path_buf(),
            session_key: session_key.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited { code: Option<i32> },
    Killed,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited { code } => code,
            Self::Killed => Some(-libc::SIGTERM),
            Self::Running => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    Missing,
}

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub owner: Owner,
    pub command: String,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub retained_chars: usize,
}

impl SpawnSpec {
    pub fn with_default_bounds(
        owner: Owner,
        command: String,
        program: OsString,
        args: Vec<OsString>,
        cwd: PathBuf,
        env: BTreeMap<OsString, OsString>,
    ) -> Self {
        Self {
            owner,
            command,
            program,
            args,
            cwd,
            env,
            retained_chars: DEFAULT_RETAINED_CHARS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Spawned {
    pub id: String,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: Status,
    pub pid: Option<u32>,
    pub started_at: SystemTime,
    pub uptime: Duration,
    pub output_preview: String,
}

#[derive(Clone, Debug)]
pub struct Poll {
    pub id: String,
    pub command: String,
    pub status: Status,
    pub pid: Option<u32>,
    pub uptime: Duration,
    pub output_preview: String,
}

#[derive(Clone, Debug)]
pub struct Log {
    pub id: String,
    pub command: String,
    pub status: Status,
    pub output: String,
    pub total_lines: usize,
    pub returned_lines: usize,
}

#[derive(Clone, Debug)]
pub enum WaitOutcome {
    Finished {
        status: Status,
        command: String,
        output: String,
    },
    TimedOut {
        command: String,
        output: String,
        uptime: Duration,
    },
}

#[derive(Clone, Debug)]
pub struct KillOutcome {
    pub id: String,
    pub already_finished: bool,
    pub status: Status,
    pub command: String,
    pub output: String,
}

struct OutputStore {
    cap: usize,
    chars: VecDeque<char>,
}

impl OutputStore {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            chars: VecDeque::new(),
        }
    }

    fn append(&mut self, text: &str) {
        self.chars.extend(text.chars());
        while self.chars.len() > self.cap {
            self.chars.pop_front();
        }
    }

    fn text(&self) -> String {
        self.chars.iter().collect::<String>().replace("\r\n", "\n")
    }
}

struct Process {
    owner: Owner,
    id: String,
    command: String,
    cwd: PathBuf,
    pid: Option<u32>,
    started_at: SystemTime,
    started: Instant,
    output: Arc<Mutex<OutputStore>>,
    status_rx: watch::Receiver<Option<Status>>,
    finished_at: Arc<Mutex<Option<Instant>>>,
    leader_exited: Arc<AtomicBool>,
    kill_requested: Arc<AtomicBool>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl Process {
    fn status(&self) -> Status {
        self.status_rx.borrow().unwrap_or(Status::Running)
    }

    fn output(&self) -> String {
        self.output.lock().unwrap().text()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            status: self.status(),
            pid: self.pid,
            started_at: self.started_at,
            uptime: self.started.elapsed(),
            output_preview: tail_chars(&self.output(), 200),
        }
    }
}

pub struct Registry {
    processes: Mutex<Vec<Arc<Process>>>,
    sequence: AtomicU64,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(0),
        }
    }

    pub fn spawn(&self, spec: SpawnSpec) -> Result<Spawned, String> {
        self.prune()?;
        let id = self.mint_id(&spec.owner, &spec.command);
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let output = Arc::new(Mutex::new(OutputStore::new(spec.retained_chars)));
        let mut stdout_task = tokio::spawn(capture(stdout, output.clone()));
        let mut stderr_task = tokio::spawn(capture(stderr, output.clone()));
        let (status_tx, status_rx) = watch::channel(None);
        let finished_at = Arc::new(Mutex::new(None));
        let finished_stamp = finished_at.clone();
        let leader_exited = Arc::new(AtomicBool::new(false));
        let leader_exit = leader_exited.clone();
        let kill_requested = Arc::new(AtomicBool::new(false));
        let killed = kill_requested.clone();
        let supervisor = tokio::spawn(async move {
            let waited = child.wait().await;
            leader_exit.store(true, Ordering::Release);
            let stdout_settled = settle(&mut stdout_task).await;
            let stderr_settled = settle(&mut stderr_task).await;
            if !stdout_settled || !stderr_settled {
                signal_group(pid, libc::SIGKILL);
            }
            let status = if killed.load(Ordering::Acquire) {
                Status::Killed
            } else {
                match waited {
                    Ok(status) => Status::Exited {
                        code: exit_code(&status),
                    },
                    Err(_) => Status::Exited { code: None },
                }
            };
            *finished_stamp.lock().unwrap() = Some(Instant::now());
            let _ = status_tx.send(Some(status));
        });
        let process = Arc::new(Process {
            owner: spec.owner,
            id: id.clone(),
            command: spec.command,
            cwd: spec.cwd,
            pid,
            started_at: SystemTime::now(),
            started: Instant::now(),
            output,
            status_rx,
            finished_at,
            leader_exited,
            kill_requested,
            supervisor: Mutex::new(Some(supervisor)),
        });
        self.processes.lock().unwrap().push(process);
        Ok(Spawned { id, pid })
    }

    pub fn list(&self, owner: &Owner) -> Vec<Snapshot> {
        let _ = self.prune();
        self.processes
            .lock()
            .unwrap()
            .iter()
            .filter(|process| process.owner == *owner)
            .map(|process| process.snapshot())
            .collect()
    }

    pub fn poll(&self, owner: &Owner, id: &str) -> Result<Poll, ResolveError> {
        let process = self.resolve(owner, id)?;
        Ok(Poll {
            id: process.id.clone(),
            command: process.command.clone(),
            status: process.status(),
            pid: process.pid,
            uptime: process.started.elapsed(),
            output_preview: tail_chars(&process.output(), 1_000),
        })
    }

    pub fn log(
        &self,
        owner: &Owner,
        id: &str,
        offset: Option<usize>,
        limit: usize,
    ) -> Result<Log, ResolveError> {
        let process = self.resolve(owner, id)?;
        let full = process.output();
        let lines: Vec<_> = full.lines().collect();
        let total_lines = lines.len();
        let (start, end) = match offset {
            Some(offset) => {
                let start = offset.min(total_lines);
                (start, start.saturating_add(limit).min(total_lines))
            }
            None => (total_lines.saturating_sub(limit), total_lines),
        };
        let output = lines[start..end].join("\n");
        Ok(Log {
            id: process.id.clone(),
            command: process.command.clone(),
            status: process.status(),
            returned_lines: end - start,
            output,
            total_lines,
        })
    }

    pub async fn wait(
        &self,
        owner: &Owner,
        id: &str,
        timeout: Duration,
    ) -> Result<WaitOutcome, ResolveError> {
        let process = self.resolve(owner, id)?;
        let mut status = process.status_rx.clone();
        let finished = tokio::time::timeout(timeout, async {
            loop {
                if let Some(status) = *status.borrow_and_update() {
                    return status;
                }
                if status.changed().await.is_err() {
                    return Status::Exited { code: None };
                }
            }
        })
        .await;
        Ok(match finished {
            Ok(status) => WaitOutcome::Finished {
                status,
                command: process.command.clone(),
                output: tail_chars(&process.output(), 2_000),
            },
            Err(_) => WaitOutcome::TimedOut {
                command: process.command.clone(),
                output: tail_chars(&process.output(), 1_000),
                uptime: process.started.elapsed(),
            },
        })
    }

    pub async fn kill(&self, owner: &Owner, id: &str) -> Result<KillOutcome, ResolveError> {
        let process = self.resolve(owner, id)?;
        let before = process.status();
        let mut kill_sent = false;
        if !before.is_terminal() {
            if !process.leader_exited.load(Ordering::Acquire) {
                process.kill_requested.store(true, Ordering::Release);
                signal_group(process.pid, libc::SIGTERM);
                kill_sent = true;
            }
            if matches!(
                self.wait(owner, &process.id, KILL_GRACE).await?,
                WaitOutcome::TimedOut { .. }
            ) {
                process.kill_requested.store(true, Ordering::Release);
                signal_group(process.pid, libc::SIGKILL);
                kill_sent = true;
                let _ = self.wait(owner, &process.id, KILL_GRACE).await;
            }
        }
        Ok(KillOutcome {
            id: process.id.clone(),
            already_finished: before.is_terminal() || !kill_sent,
            status: process.status(),
            command: process.command.clone(),
            output: tail_chars(&process.output(), 2_000),
        })
    }

    pub fn has_active_for_session(
        &self,
        profile_home: &Path,
        session_key: &str,
        max_age: Option<Duration>,
    ) -> bool {
        self.processes.lock().unwrap().iter().any(|process| {
            process.owner.profile_home == profile_home
                && process.owner.session_key == session_key
                && process.status() == Status::Running
                && max_age.is_none_or(|limit| process.started.elapsed() < limit)
        })
    }

    pub async fn shutdown(&self) {
        let processes = self.processes.lock().unwrap().clone();
        for process in &processes {
            if process.status() == Status::Running && !process.leader_exited.load(Ordering::Acquire)
            {
                process.kill_requested.store(true, Ordering::Release);
                signal_group(process.pid, libc::SIGTERM);
            }
        }
        wait_for_all(&processes, KILL_GRACE).await;
        for process in &processes {
            if process.status() == Status::Running {
                process.kill_requested.store(true, Ordering::Release);
                signal_group(process.pid, libc::SIGKILL);
            }
        }
        wait_for_all(&processes, KILL_GRACE).await;
    }

    fn resolve(&self, owner: &Owner, id: &str) -> Result<Arc<Process>, ResolveError> {
        let query = id.trim();
        if query.is_empty() {
            return Err(ResolveError::Missing);
        }
        let query = if query.starts_with("proc_") {
            query.to_owned()
        } else {
            format!("proc_{query}")
        };
        let processes = self.processes.lock().unwrap();
        if let Some(process) = processes
            .iter()
            .find(|process| process.owner == *owner && process.id == query)
        {
            return Ok(process.clone());
        }
        if query.len().saturating_sub("proc_".len()) < MIN_PREFIX_CHARS {
            return Err(ResolveError::Missing);
        }
        let mut matches = processes
            .iter()
            .filter(|process| process.owner == *owner && process.id.starts_with(&query));
        match (matches.next(), matches.next()) {
            (Some(process), None) => Ok(process.clone()),
            _ => Err(ResolveError::Missing),
        }
    }

    fn prune(&self) -> Result<(), String> {
        let mut processes = self.processes.lock().unwrap();
        processes.retain(|process| {
            process.status() == Status::Running
                || process
                    .finished_at
                    .lock()
                    .unwrap()
                    .is_none_or(|finished| finished.elapsed() <= FINISHED_TTL)
        });
        while processes.len() >= MAX_PROCESSES {
            let Some(index) = processes
                .iter()
                .position(|process| process.status().is_terminal())
            else {
                return Err(format!(
                    "Background process limit reached ({MAX_PROCESSES} active processes)."
                ));
            };
            processes.remove(index);
        }
        Ok(())
    }

    fn mint_id(&self, owner: &Owner, command: &str) -> String {
        loop {
            let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let material = format!(
                "{}\0{}\0{}\0{}\0{command}",
                std::process::id(),
                sequence,
                stamp,
                owner.profile_home.display()
            );
            let digest = format!("{:x}", Sha256::digest(material.as_bytes()));
            let id = format!("proc_{}", &digest[..12]);
            if !self
                .processes
                .lock()
                .unwrap()
                .iter()
                .any(|process| process.id == id)
            {
                return id;
            }
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        if let Ok(processes) = self.processes.lock() {
            for process in processes.iter() {
                if process.status() == Status::Running {
                    process.kill_requested.store(true, Ordering::Release);
                    signal_group(process.pid, libc::SIGKILL);
                }
                if let Ok(mut supervisor) = process.supervisor.lock() {
                    if let Some(supervisor) = supervisor.take() {
                        supervisor.abort();
                    }
                }
            }
        }
    }
}

async fn capture(mut stream: impl tokio::io::AsyncRead + Unpin, output: Arc<Mutex<OutputStore>>) {
    let mut chunk = [0_u8; 8_192];
    let mut pending = Vec::new();
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                let tail = decode_available(&mut pending, true);
                if !tail.is_empty() {
                    output.lock().unwrap().append(&tail);
                }
                return;
            }
            Ok(count) => {
                pending.extend_from_slice(&chunk[..count]);
                let text = decode_available(&mut pending, false);
                if !text.is_empty() {
                    output.lock().unwrap().append(&text);
                }
            }
        }
    }
}

fn decode_available(pending: &mut Vec<u8>, final_chunk: bool) -> String {
    let mut decoded = String::new();
    let mut consumed = 0;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(text) => {
                decoded.push_str(text);
                consumed = pending.len();
            }
            Err(error) => {
                let valid_end = consumed + error.valid_up_to();
                decoded.push_str(
                    std::str::from_utf8(&pending[consumed..valid_end])
                        .expect("valid_up_to marks a verified UTF-8 prefix"),
                );
                consumed = valid_end;
                let Some(error_len) = error.error_len() else {
                    break;
                };
                decoded.push('\u{fffd}');
                consumed += error_len;
            }
        }
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
    if final_chunk && !pending.is_empty() {
        decoded.push_str(&String::from_utf8_lossy(pending));
        pending.clear();
    }
    decoded
}

async fn settle(task: &mut JoinHandle<()>) -> bool {
    if tokio::time::timeout(CAPTURE_SETTLE, &mut *task)
        .await
        .is_ok()
    {
        true
    } else {
        task.abort();
        let _ = task.await;
        false
    }
}

async fn wait_for_all(processes: &[Arc<Process>], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while processes
        .iter()
        .any(|process| process.status() == Status::Running)
    {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(25))).await;
    }
}

fn tail_chars(text: &str, count: usize) -> String {
    let start = text
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    text[start..].to_owned()
}

fn exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| -signal)
    }
    #[cfg(not(unix))]
    None
}

fn signal_group(pid: Option<u32>, signal: libc::c_int) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: negative pid addresses only the child-owned process group.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn owner(name: &str) -> Owner {
        Owner::new(Path::new("/profile"), name)
    }

    fn spec(owner: Owner, command: &str) -> SpawnSpec {
        let mut env = BTreeMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            env.insert("PATH".into(), path);
        }
        SpawnSpec::with_default_bounds(
            owner,
            command.into(),
            "/bin/bash".into(),
            vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                command.into(),
            ],
            std::env::temp_dir(),
            env,
        )
    }

    async fn finished(registry: &Registry, owner: &Owner, id: &str) -> Status {
        match registry
            .wait(owner, id, Duration::from_secs(5))
            .await
            .unwrap()
        {
            WaitOutcome::Finished { status, .. } => status,
            WaitOutcome::TimedOut { .. } => panic!("process did not finish"),
        }
    }

    #[tokio::test]
    async fn captures_merged_output_and_natural_exit_codes() {
        let registry = Registry::new();
        let owner = owner("route");
        let spawned = registry
            .spawn(spec(owner.clone(), "printf out; printf err >&2; exit 7"))
            .unwrap();
        assert_eq!(
            finished(&registry, &owner, &spawned.id).await,
            Status::Exited { code: Some(7) }
        );
        let poll = registry.poll(&owner, &spawned.id).unwrap();
        assert!(poll.output_preview.contains("out"));
        assert!(poll.output_preview.contains("err"));
        assert_eq!(poll.status.exit_code(), Some(7));
    }

    #[tokio::test]
    async fn poll_repeats_the_current_tail_and_log_pages_lines() {
        let registry = Registry::new();
        let owner = owner("route");
        let spawned = registry
            .spawn(spec(owner.clone(), "printf 'one\\ntwo\\nthree\\n'"))
            .unwrap();
        finished(&registry, &owner, &spawned.id).await;
        let first = registry.poll(&owner, &spawned.id).unwrap().output_preview;
        let second = registry.poll(&owner, &spawned.id).unwrap().output_preview;
        assert_eq!(first, second);
        assert_eq!(
            registry
                .log(&owner, &spawned.id, Some(0), 2)
                .unwrap()
                .output,
            "one\ntwo"
        );
        assert_eq!(
            registry.log(&owner, &spawned.id, None, 2).unwrap().output,
            "two\nthree"
        );
    }

    #[tokio::test]
    async fn retained_output_is_bounded() {
        let registry = Registry::new();
        let owner = owner("route");
        let mut request = spec(owner.clone(), "printf 1234567890");
        request.retained_chars = 4;
        let spawned = registry.spawn(request).unwrap();
        finished(&registry, &owner, &spawned.id).await;
        assert_eq!(
            registry.log(&owner, &spawned.id, None, 10).unwrap().output,
            "7890"
        );
    }

    #[test]
    fn finished_retention_is_measured_from_exit_not_spawn() {
        let registry = Registry::new();
        let owner = owner("route");
        let id = "proc_000000000001".to_owned();
        let (_, status_rx) = watch::channel(Some(Status::Exited { code: Some(0) }));
        registry.processes.lock().unwrap().push(Arc::new(Process {
            owner: owner.clone(),
            id: id.clone(),
            command: "true".into(),
            cwd: std::env::temp_dir(),
            pid: None,
            started_at: SystemTime::now(),
            started: Instant::now()
                .checked_sub(FINISHED_TTL + Duration::from_secs(1))
                .unwrap(),
            output: Arc::new(Mutex::new(OutputStore::new(10))),
            status_rx,
            finished_at: Arc::new(Mutex::new(Some(Instant::now()))),
            leader_exited: Arc::new(AtomicBool::new(true)),
            kill_requested: Arc::new(AtomicBool::new(false)),
            supervisor: Mutex::new(None),
        }));
        registry.prune().unwrap();
        assert!(registry.poll(&owner, &id).is_ok());
    }

    #[test]
    fn retained_output_is_character_bounded_and_utf8_decoding_is_incremental() {
        let mut output = OutputStore::new(4);
        let mut pending = vec![0xe7, 0x8c];
        assert_eq!(decode_available(&mut pending, false), "");
        pending.extend_from_slice(&[0xab, b'a', b'b', b'c', b'd']);
        output.append(&decode_available(&mut pending, false));
        assert_eq!(output.text(), "abcd");
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn owner_and_prefix_resolution_are_isolated() {
        let registry = Registry::new();
        let first = owner("first");
        let second = owner("second");
        let spawned = registry.spawn(spec(first.clone(), "true")).unwrap();
        finished(&registry, &first, &spawned.id).await;
        let suffix = spawned.id.strip_prefix("proc_").unwrap();
        assert!(registry.poll(&first, &suffix[..4]).is_ok());
        assert_eq!(
            registry.poll(&first, &suffix[..3]).unwrap_err(),
            ResolveError::Missing
        );
        assert_eq!(
            registry.poll(&second, &spawned.id).unwrap_err(),
            ResolveError::Missing
        );
    }

    #[tokio::test]
    async fn kill_terminates_the_owned_process_group() {
        let registry = Registry::new();
        let owner = owner("route");
        let spawned = registry
            .spawn(spec(owner.clone(), "sleep 30 & wait"))
            .unwrap();
        assert!(registry.has_active_for_session(
            Path::new("/profile"),
            "route",
            Some(Duration::from_secs(60))
        ));
        let killed = registry.kill(&owner, &spawned.id).await.unwrap();
        assert!(!killed.already_finished);
        assert_eq!(killed.status, Status::Killed);
        assert!(!registry.has_active_for_session(Path::new("/profile"), "route", None));
    }

    #[tokio::test]
    async fn supplied_environment_is_authoritative() {
        let registry = Registry::new();
        let owner = owner("route");
        let mut request = spec(owner.clone(), "printf '%s:%s' \"$KEPT\" \"${HOME-unset}\"");
        request.env.insert("KEPT".into(), "profile".into());
        let spawned = registry.spawn(request).unwrap();
        finished(&registry, &owner, &spawned.id).await;
        assert_eq!(
            registry.poll(&owner, &spawned.id).unwrap().output_preview,
            "profile:unset"
        );
    }
}
