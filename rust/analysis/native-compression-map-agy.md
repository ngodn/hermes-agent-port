# Native compression source map from AGY

AGY inspected the Python manual compression command, persistence path, session
store, leases, prompt lifecycle, and the corresponding Rust seams before the
implementation began.

Its conservative recommendation was to ship grammar and preview first unless
Rust gained all of the following real surfaces: a tool-free summary call, an
atomic child-first persistence transition, frozen prompt and tool inheritance,
safe client eviction, and explicit handling for unavailable memory checkpoints.
It mapped the accepted flags and `here N` boundary behavior and warned against
presenting hard truncation as `--aggressive` compression.

The final implementation went beyond the preview-only recommendation because a
second audit established that the existing native `ChatModel::step` path could
perform the required no-tools summary call. AGY's safety conditions were kept as
acceptance criteria and were later checked again against the working diff.
