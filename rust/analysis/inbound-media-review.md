# Inbound-media classification: parity review

Reviewer: Opus 4.8 medium helper. Scope: `inbound_media.rs`, `inbound_media_parity.rs`,
`gen_inbound_media_goldens.py`. Compared against the five predicates at
`gateway/run.py:3616-3665` and the classification loop at `gateway/run.py:20579-20600`.
I did not run cargo; findings are from source reading. Codex owns fixes.

## Verdict

The port is faithful. I found no correctness defect in the five predicates or in
`classify_media`. Each Rust branch maps 1:1 to the Python it cites, including the
truthy-MIME precedence, the `startswith` case/whitespace sensitivity, the AUDIO/DOCUMENT
STT exclusion, and the `_pending_stt_prepared` gate. All 9 `MessageType` values
(`base.py:2494-2502`) exist in the Rust enum with matching `from_value` strings, so the
differential harness exercises every type rather than panicking on `unwrap`.

## Coverage: strengths

- The oracle AST-extracts the real loop node and the five real `FunctionDef` nodes and
  execs them (`gen_inbound_media_goldens.py:24-43`). It is not a hand transcription, so a
  logic edit in `run.py` changes the goldens, and `--check` fails on drift when run.
  Codex correction: this generator check is not currently wired into CI.
- 217 cases sweep all message types x ragged `media_types` layouts x cached=False/True,
  with duplicate paths (no-dedup check), a unicode path, and one out-of-range slot
  (`range(len(paths)+1)`), covering the `""` fallback and the message-type fallthrough.
- The video branch is inlined in Python (`run.py:20599`, it does not call
  `_event_media_is_video`) while Rust `classify_media` calls the predicate. These are
  logically equivalent and the goldens compare the two forms, so the equivalence is
  actually verified. Do not "fix" the Rust to inline it.

## Limits of the AST-extracted differential tests (state these plainly)

1. Only the inner `for` node is transplanted into `classify` (line 39). Pre-loop init and
   any post-loop work the real `_prepare_inbound_message_text` does around it (dedup,
   normalization, the image-routing / STT / document enrichment at `run.py:20602+`) are not
   in the oracle. The goldens prove loop-body parity, not full-method parity. That matches
   the deliberate slice boundary in `inbound_media.rs:12-16`, but it is a boundary, not
   total coverage.
2. Extraction is anchored on exact source strings (`ast.unparse(n.test) == "event.media_urls"`
   and `enumerate(event.media_urls)`, lines 31-33). A refactor renaming those raises
   StopIteration and fails loudly rather than drifting silently. Fail-closed is good, but it
   means the harness cannot follow a semantically-equivalent rewrite without editing the tool.
3. The exec scope is only `{"Enum": enum.Enum}` (line 41). It works today because the five
   predicates reference nothing but `MessageType` and `str`. If a predicate later reads a
   module global (e.g. an audio-MIME set), generation NameErrors. Again loud, not silent.

## Minor coverage gaps (not defects)

- Predicates are sampled only for slots `0..=6` while `media_types` layouts run to length 9,
  so MIME variety at indices 7-8 is never asserted (the loop never reaches them either).
- No fixture and no unit test lands one attachment in two buckets, so the "independent ifs,
  can appear in more than one bucket" note (`inbound_media.rs:104-108`) is unverified. Given a
  single `message_type` and mutually-exclusive MIME prefixes it is in fact unreachable, so the
  comment slightly overstates reachability. Consider softening it to say "unreachable in
  practice, structure preserved for fidelity."

Codex disposition: corrected the bucket comment. MIME rotations cover the
values that initially occupy slots 7-8 in other cases; no behavior change was
needed. Full workspace tests and clippy passed. This review ran successfully
with the user's requested permission bypass and reported `claude-opus-4-8`.
