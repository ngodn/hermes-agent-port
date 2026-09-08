# Native compression source map from Claude

Claude independently traced the Python gateway handler, context compressor,
session rotation, title and cache behavior, feedback, and the current Rust
provider boundary.

It found that Rust already had enough provider functionality for an honest
rotation-mode manual compression slice: `ChatModel::step` can issue one
non-streaming request with no tools, and Python legitimately falls back to the
main conversation model when no auxiliary compression route is configured. It
recommended persisting a summary plus an assistant waiting handoff into a new
child, copying the protected tail, closing the parent last, and evicting only
the old physical conversation client.

It also separated later work from this checkpoint: default in-place
compaction, auxiliary compression routing and fallback/cooldown policy,
automatic threshold-driven compression, memory checkpoint hooks, context-engine
notifications, and the full Python compressor's pruning heuristics.
