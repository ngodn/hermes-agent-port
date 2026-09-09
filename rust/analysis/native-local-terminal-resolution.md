# Native local foreground terminal resolution

## Outcome

The native gateway now owns a production Unix-local foreground `terminal` tool for
conversations that explicitly set `terminal.backend: local` and
`approvals.mode: off`, have no user deny rules, and enable the existing native
tool opt-in. Other approval modes and execution backends do not advertise this
native tool. Their compatibility route remains the Python agent until the full
policies are native.

The tool is constructed once per conversation and enters the same frozen tool
prefix as every other native capability. A live provider integration performs
two terminal calls in one tool loop and proves that the schema is identical on
all three provider requests.

## Runtime boundary

`foreground_exec` owns the child process lifecycle behind one typed `run`
operation. It clears the inherited environment, starts a new process group,
drains both pipes concurrently, retains only bounded head and tail windows,
spills overflow to unique private files, reaps the direct child, and kills the
whole group on timeout or inherited-pipe linger. Typed outcomes distinguish a
normal exit, spawn failure, and timeout while retaining bounded partial output.

`TerminalTool` owns the conversation state behind the existing `Tool` trait. It
serializes calls, starts from the selected profile's isolated environment,
persists exported shell variables in a private snapshot, tracks the observed
working directory, strips ANSI sequences, redacts visible and spilled output,
and returns command failures as structured tool values rather than transport
errors.

Working-directory persistence targets the session currently named by the
durable gateway route in the same SQLite statement. If compression rotates the
route, later commands update the child and cannot write back to the ended
parent. An explicit per-call `workdir` remains transient.

## Security boundary

The native tool is hidden unless interactive approval is unnecessary and no
unported user deny policy exists. Even then, the hardline floor is mandatory.
The shell-aware scanner blocks root, home, and protected-system recursive
deletes, filesystem formatting, raw-device writes, fork bombs, process-wide
kills, host shutdown commands, and `sudo -S` password guessing. It scans shell
carriers and command substitutions while leaving quoted prose alone.

The checked-in Python oracle source-executes 233 reference cases without
running candidate commands. Rust tests consume its hardline, sudo, workdir,
provider-rejection, and command/timeout-rejection slices, 147 cases in total.
The remaining 86 background, successful-execution, and approval-classification
cases are retained for later native checkpoints.

Output spill files are created with exclusive names under a mode `0700`
directory and mode `0600` files. Raw spill content is rewritten through the
same ANSI and secret scrubbers before the path is exposed. A failed rewrite
deletes the spill.

## Helper lanes and review disposition

AGY ran once behind the repository authentication lock and owned only the
source-executed Python terminal and approval oracle. Claude independently owned
only the first foreground-execution draft. The primary lane reviewed both,
replaced Claude's unbounded `read_to_end` capture, removed fixed spill-name
collisions and lint suppression, added successful-parent descendant cleanup,
implemented the security and terminal layers, integrated the live tool, and
ran final validation. The lanes did not duplicate assignments.

The codebase-design skill kept the process machinery behind one narrow typed
interface and the per-conversation state behind one provider-visible tool.

## Explicit deferrals

- Smart, ask, manual, cron, single-query, and unattended approval workflows.
- User deny glob matching and permanent approval storage.
- Managed background processes, PTYs, notifications, and the process tool.
- Docker, SSH, Singularity, Modal, Daytona, Vercel, and plugin backends.
- Windows shell discovery and escaped-descendant sweeping outside the process
  group.
- Terminal failure hints, verification evidence, lifecycle output transforms,
  sudo prompting and cache handling, and the complete Python redaction
  heuristics.

These are not silently approximated. Configuration that needs them does not
receive the native terminal tool.
