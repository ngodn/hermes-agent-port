# Native main-provider retry notices

## Outcome

Ordinary native chat-completions turns now preserve Python's retry presentation
lifecycle. Same-route countdowns, fallback-attempt context, durable model
switches, and the final terminal status share one ordered turn-local trace.
Successful recovery drops transient records, terminal failure flushes them once,
and successful fallback still emits only the durable switch banner.

Z.AI Coding overloads keep the existing split behavior. Short retry notices are
buffered like ordinary countdowns. Adaptive long-backoff notices are sent live
before sleeping so a multi-minute wait does not look frozen. Live delivery is
best effort and cannot block or abort provider recovery.

## Runtime integration

`main_provider_notices.rs` now accepts transient records in the same buffer that
already held durable fallback copies. Its recovery drain clears that buffer and
takes only durable records. Its terminal drain discards the separate durable
copies and takes the full trace in insertion order.

`NativeAgentClient::wait_before_main_retry` calculates the actual wait before it
records the exact retry numerator and configured attempt ceiling. Ordinary
retries use Python's `Retrying in` sentence. Z.AI overload retries use the
provider-overloaded sentence and the same short or long policy suffix. Frozen
fallback route clones share the turn-local notice state and live sender, so
route changes do not lose ordering.

Fallback activation records the branch-specific context line before the model
switch banner. Transport and overload failures report an unreachable provider,
generic server exhaustion reports the exhausted retry budget, and non-retryable
format rejection includes the HTTP status. The live Python oracle also proved
that the format-error path calls fallback activation without its classifier
reason. Rust therefore uses `provider failure` in that switch banner instead of
the previously inferred `request format rejected`.

At final pre-body HTTP failure, Rust extracts a bounded one-line provider detail,
scrubs secrets, and records the terminal status after all retry and switch
records. Transport terminal errors receive the same bounded presentation path.
These are `GatewayNotice` events only. They never enter the reply, SQLite,
history, prompt, tool schema, or provider request.

## Behavioral proof

Public Rust tests use local HTTP servers and prove:

- HTTP 500 followed by success makes two identical-shape requests and emits no
  retry notice
- terminal HTTP 502 makes three requests and emits retry 1/3, retry 2/3, then
  the terminal status exactly once
- ordinary HTTP 503 makes two primary requests, switches once, makes three
  fallback requests, and flushes the complete status trace in order
- deterministic HTTP 500 format rejection makes one request per route and uses
  the live Python `provider failure` switch reason
- unrelated terminal HTTP failures still emit the non-retryable status even
  though they are not eligible for fallback
- short Z.AI waits stay buffered while long adaptive waits are sent live
- provider-derived terminal details are bounded and secret-redacted
- transient and durable buffer drains are ordered and idempotent

AGY executed four real `AIAgent.run_conversation()` cases with patched wire I/O
and zero-duration waits. It captured every status and diagnostic buffer record,
the clear or flush boundary, terminal emission, and per-route call count. Its
first broader assignment exceeded the helper's 15-minute print limit before
writing artifacts, so the second assignment was narrowed to one report and
completed within the bound. The live report corrected the format-reason nuance
above.

Claude independently mapped interrupted retry waits, stale attribution, run
budget accounting, cancellation, and steering. That work did not overlap the
notice implementation and is reserved for the next checkpoint.

Claude then reviewed the finished notice diff. It found no cache, transcript,
secret, mutex, sender-ownership, or retry-budget defect. It raised three
presentation concerns that primary review checked against the live Python
trace. Python deliberately displays the configured `max_retries` denominator
even when eager fallback stops a route after two failures, deliberately uses
the failed-attempt numerator for ordinary waits and the upcoming-attempt
numerator for Z.AI overload waits, and deliberately emits long Z.AI waits live
without buffering a second copy. Rust keeps all three reference behaviors. The
review's transport-error redaction gap received a direct regression test.

## Scope boundary

This checkpoint covers ordinary pre-body chat-completions retry notices and the
already-implemented Z.AI adaptive backoff schedule. Python's verbose per-attempt
console diagnostics are not promoted into messaging notices and remain outside
this presentation-event checkpoint. Provider-response heartbeats, managed local-model load progress,
stream-stall reconnect notices, payload repair, compression, OAuth-specific
refresh messages, and cooperative interrupted-wait accounting remain separate
work.

## Verification

- Full Rust workspace: 1,965 passed, two expected ignores
- Focused live Python support suite: 17 passed
- Rust formatting and workspace Clippy with warnings denied: passed
- Added-line em-dash check and `git diff --check`: passed

## Progress

The capability inventory moves native agent core from 85% to 86%. Gateway,
tool/RPC, and state/search estimates are unchanged. With the stable weights:

`0.35 * 67 + 0.30 * 25 + 0.15 * 76 + 0.20 * 86 = 59.55`

The refreshed estimate is **59.55%, reported as about 60%**, with a judgment
range of 56% to 62%. This remains an engineering inventory of production
behavior, not a file, line, commit, or test-count ratio.
