# Slack private audio downloads, 2026-09-06

Source: `plugins/platforms/slack/adapter.py::_resolve_slack_audio_ext`,
`_is_slack_voice_clip`, `_download_slack_file`, and the file branches around
line 6842. The [Slack file-object contract](https://docs.slack.dev/reference/objects/file-object/)
requires bearer authentication for private file downloads.

Socket Mode continues to acknowledge the envelope before preparing its message.
User `file_share` messages are now accepted; empty text is retained when a file
is audio or a video-labeled voice clip. Other message subtypes and bot events
remain filtered. Filename extensions take precedence over MIME defaults, and
the shared cache sniffs the actual downloaded container before choosing its
stored extension. 216 cases execute the real Python extension and voice-marker
functions, covering hidden filenames, unsupported extensions, MIME case and
parameters, and video versus voice markers.

Startup supplies the active profile and configured inbound size cap. The adapter
selects url_private_download before url_private, resolves an HTTPS Slack CDN
hostname, validates every resolved IP and pins those addresses in the request
client. The hostname remains intact for TLS validation. Media requests bypass
environment proxies and automatic redirects, and use the bot token, never the
Socket Mode app token. HTML responses are rejected before caching. The shared
reader rejects non-success responses and bounds declared and streamed sizes.
Download errors produce a generic caption note without URLs or credentials.

The inline HTTP test covers the authenticated fetch/cache effect with a local
server, asserting the exact synthetic bot bearer, real file bytes and sniffed
m4a extension. HTML, redirects and size errors produce no path. Separate tests
exercise production URL/IP validation, including lookalikes, userinfo, private
and mapped addresses, metadata addresses and CGNAT. A refused URL passes through
message preparation and preserves the caption; audio-only file_share extraction
and bot filtering are checked too. The localhost HTTP test deliberately invokes
the fetch boundary after validation; it is not an end-to-end test of production
DNS resolution, TLS or Socket Mode against Slack.

Remaining scope: lifecycle file-ID events with shared-file deduplication;
multiple-workspace token routing; Python's
guarded redirect/retry behavior and exact attachment-failure notices; full
thread/mention/group filtering and other attachments. IPv6 address admission
currently restricts non-mapped addresses to public global-unicast space and
does not port the full configurable Python URL-safety policy. Production proxy
support requires preserving the DNS-pinning boundary. Full Slack adapter parity
is not claimed.

## Slack Connect metadata resolution

The normal message path now follows the Python branch at adapter.py:6800:
only `file_access="check_file_info"` entries trigger files.info; a missing ID
skips the entry, a successful lookup replaces the stub, and errors skip it with
a generic user-visible notice. Event identity/kind/bot filtering happens before
network work. Classification then uses resolved metadata, so a captionless stub
can become a voice clip instead of being dropped early. The source event remains
unchanged, and resolved files retain their order.

The request uses GET files.info with a query-encoded file ID and the bot bearer,
following the [official method contract](https://docs.slack.dev/reference/methods/files.info/).
The API client now rejects automatic redirects. Tests use a local HTTP server
to verify token choice, query IDs, lookup order, complete-entry bypass,
missing-ID skip, source-event immutability, lookup failures and bot filtering.
A resolved video-labeled voice clip reaches the download admission step; its
fixture URL is deliberately rejected without a second request.

Source inspection corrected the next-step scope: `_handle_slack_file_shared`
fetches metadata but processes only video files. It waits 0.75 seconds and checks
workspace/share timestamp deduplication before synthesizing a message. That
coupled lifecycle path remains unported. Do not turn file_shared into a generic
audio trigger: doing so would duplicate normal message.file_share turns.
Detailed Slack error notices and multiple-workspace client selection also remain.

## Normal-message redelivery deduplication

The adapter now owns the already-ported platform_helpers::MessageDeduplicator,
with max_size 2000 and Slack's one-hour default TTL. It retains that instance
across Socket Mode reconnects. A short mutex-protected claim precedes metadata
lookup and media downloads; no lock is held during network awaits. Empty
timestamps remain unclaimed. The changed-event timestamp is preferred where
present, though edited-event normalization itself remains unsupported.

Thirty cases execute Python's actual `_event_team_id` and `_workspace_event_id`
helpers and compare keys, including nested team objects and authorization
fallback. Tests show concurrent same-key preparation admits one message,
different workspaces both pass, missing timestamps remain repeatable, and a
redelivered share performs no second files.info call. Basic TTL override parsing
is also covered. Shared cache expiry/eviction behavior retains its existing
inline tests. Claims remain on download failure, as in the Python message path.

Standalone file_shared still needs the source's video-only handler and delayed
share timestamp arbitration using this same cache. Do not claim lifecycle
deduplication complete merely because normal-message redelivery is handled.

Validation: 1,256 workspace tests passed (one core and 1,255 gateway), two
ignored by default. Clippy with warnings denied, formatting, fixture `--check`
and diff whitespace checks pass. Local logs:
`/tmp/hermes-slack-dedup-final.log` and
`/tmp/hermes-slack-dedup-clippy.log`.


## Normal video shares

The regular message path now accepts captionless genuine videos. Video-labeled
voice clips still go to audio transcription. Suffix selection runs against 144
cases extracted from the actual Python Slack video branch and video type table.

Authenticated downloads share the audio destination checks and bounded body
reader. Video writes use `cache/videos` or the populated legacy `video_cache`,
and retain the video suffix instead of applying audio container sniffing.
Local HTTP tests verify MP4 bytes, cache location, and invalid suffix rejection;
existing HTTP failure tests exercise the shared authentication/response boundary.
The local fixture bypasses production HTTPS/DNS admission explicitly.

`Message.video_paths` defaults empty for old JSON payloads and survives serde
round trips. Dispatcher prepends the existing video context note after slash
handling. Its inline test verifies that the cached path reaches the agent and
that `/help` with a video still dispatches as a command. Download and Dispatcher
checks are separate tests, not a live Slack end-to-end demonstration.

The delayed `file_shared` lifecycle fallback remains unimplemented. Its 750 ms
wait must allow regular share events to claim the shared dedup cache; adding a
sleep to the current serial Socket Mode loop would prevent that. Native video
inspection tools, remote agent path translation, and cache cleanup also remain.

Validation: 1,258 workspace tests passed, two ignored. Clippy with warnings
denied, formatting, Python fixture regeneration and diff whitespace checks pass.
Logs: `/tmp/hermes-slack-video-workspace.log` and
`/tmp/hermes-slack-video-clippy.log` (temporary, not durable artifacts).


## Video lifecycle fallback

`file_shared` now fetches metadata through the shared `files.info` implementation.
It only proceeds for video MIME types, picks the matching channel's first share
(or the first available share), and falls back to the lifecycle timestamp. After
750 ms it uses normal message preparation, including the same workspace dedup
claim. Video-labeled voice clips still take the audio branch after this gate.

Socket Mode now polls pending handlers alongside frames. On socket termination,
acknowledged handlers finish before reconnecting so dedup claims do not strand
unfinished turns. This does not add durable crash recovery or a concurrency cap.

The inline local WebSocket/HTTP test proves normal-event acknowledgement and
caption delivery during the grace period, suppression of its fallback, delivery
of a lifecycle-only video, suppression of repeated lifecycle events, and delivery
of acknowledged pending work after socket close. The media URL deliberately fails
admission; successful video cache writes and Dispatcher context remain covered
by the separate tests above. No live Slack account was used.

Remaining source gaps include multi-workspace tokens, ignored-channel policy,
thread routing and rich download-error notices. The earlier paragraph describing
the fallback as unimplemented is historical and superseded by this section.

Validation: 1,259 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and diff whitespace checks pass. Logs are temporary:
`/tmp/hermes-slack-lifecycle-workspace.log` and
`/tmp/hermes-slack-lifecycle-clippy.log`.


## Workspace credential routing and isolation

Startup now authenticates comma-separated bot tokens plus additional unique tokens
from the profile's `slack_tokens.json`. Saved map labels do not establish identity;
`auth.test` does. A failed registration attempt does not replace the complete map.
Normal runtime retries failed authentication without logging token-bearing errors.

Metadata requests use the event/body/authorization workspace resolver. Downloads
prefer that workspace, then `/files-pri/T...-` in the URL, then the primary token.
`Message.workspace_id` preserves reply routing. Dispatcher reply construction,
lease keys, persisted history and native request cache scope carry the same
workspace identity. Unscoped messages preserve old native session IDs. Scoped IDs
use a JSON-encoded workspace/channel pair with a distinct prefix; this is an
intermediate native identity, not a claim of full Python session-key parity.
Old unscoped Slack history is not automatically migrated.

The inline HTTP/SQLite test uses three fake credentials and a temporary profile.
It verifies authenticated identity discovery (including saved token deduplication),
metadata/download/reply Authorization headers, explicit-team versus URL fallback,
separate persisted history for identical channel IDs, and preservation of the
complete token map after failed reauthentication. No real account is used.

Remaining: Python's channel-to-team inference, complete bot/self filtering,
ignored-channel policy, native thread routing, redirect handling and full session
resolution. HTTP routing tests do not prove live multi-workspace Slack delivery.

Validation: 1,260 workspace tests passed, two ignored; Clippy with warnings denied,
formatting, regenerated existing fixtures and whitespace checks pass. Temporary
logs: `/tmp/hermes-slack-workspaces.log` and `/tmp/hermes-slack-workspaces-clippy.log`.


## Channel workspace inference

A bounded channel ownership cache now supplies missing workspace identity for
normal messages, lifecycle metadata requests and replies without explicit scope.
Conflicting team observations remove the unqualified route. Explicit metadata
continues to take precedence. The two caches independently evict oldest entries
down to half capacity, matching Python's 10,000-entry cap. Cache lookup currently
uses ordered deques; large-cache performance has not been benchmarked.

Normal messages claim their original event identity before channel inference,
matching Python's ordering. Lifecycle events claim the share timestamp with the
original event/body workspace, then clear the fallback timestamp before normal
message preparation. This avoids claiming twice after inferred routing.

Fifteen state transitions execute the actual Python ownership and trimming methods
with a small cap. They cover empty inputs, repeated ownership, ambiguity and
independent eviction. An inline async test verifies message inference, explicit
workspace routing after ambiguity, and original-identity redelivery semantics.

Validation: 1,261 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-channel-workspace.log`, `/tmp/hermes-slack-channel-clippy.log`.
Redirect handling, ignored-channel policy, bot identity filtering and full thread
routing remain incomplete.


## Ignored-channel policy

The adapter now rejects normal messages, lifecycle shares and outbound sends for
ignored channels. Thread-shaped IDs match their parent channel, wildcard ignores
all nonempty channels, and an empty channel never matches. Normal message dedup
still precedes the gate as in Python. Lifecycle events are gated before workspace
cache updates and metadata requests. Outbound suppression returns ignored_channel.

Configuration precedence follows platform extras, nonempty legacy
SLACK_IGNORED_CHANNELS, then top-level slack.ignored_channels YAML. YAML lists are
joined as in the Python plugin hook; explicit extras lists remain lists. Empty
explicit extras override the environment. Rust resolves these settings during
adapter configuration; it does not reproduce Python's per-event environment reads.

192 generated cases execute the actual Python parsing/matching methods. Inline
assertions also cover YAML precedence. The local HTTP integration asserts its
request count does not increase for an ignored Connect stub, lifecycle share or
outbound send, including a thread-shaped channel ID.

Validation: 1,262 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-ignore-workspace.log`, `/tmp/hermes-slack-ignore-clippy.log`.


## Authenticated bot identity

Workspace registration now retains auth.test user_id alongside each token and
publishes both atomically. The primary bot identity belongs to the first token,
even when a subsequent token replaces that workspace's mapped client. Failed
refreshes preserve the previous complete registry. Missing user_id remains an
unknown identity, matching the source's permissive response handling.

Normal-message preparation rejects the authenticated workspace bot's own user ID
before Connect metadata/download work, including ordinary-looking messages with
no bot_id. Channel inference supplies workspace identity when available; otherwise
the primary bot identity is used. This closes the self-echo path independently of
the currently conservative rejection of declared bot messages.

The existing local HTTP integration now verifies explicit/inferred self-sender
rejection without requests, cross-workspace identity distinction, failed refresh
retention and first-token primary identity after duplicate-workspace registration.
It does not yet verify the full allow_bots policy: users.info bot detection,
Block Kit mentions, mention-pattern gates and other-bot permission modes remain.

Validation: 1,262 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and whitespace checks pass. Tests were extended inline; no new test
function was added. Temporary logs: `/tmp/hermes-slack-self-workspace.log` and
`/tmp/hermes-slack-self-clippy.log`.


## Declared bot policy

Declared bot/app messages now use none, mentions or all policy, defaulting unknown
values to none. Platform extras win over the legacy environment and top-level
Slack YAML configuration. The api_human_users extras/environment list exempts
allowlisted user-token app posts, while explicit bot markers still identify bots.

Mentions inspect flat text and authored Block Kit user elements under elements or
element. Quoted rich-text subtrees never contribute mentions. The current native
path applies the source's primary and workspace bot-ID gates to declared senders;
self-message rejection stays unconditional. Raw parsing retains its default bot
rejection; the runtime uses the permitted-message parser after checking policy.

48 cases execute Python's declaration and mention helpers. An inline async test
covers none/all/mentions, quoted versus authored block mentions, config precedence
and self rejection even under all. Unmarked bot lookup via users.info, mention
patterns, full Block Kit body extraction and events without user IDs remain gaps.

Validation: 1,263 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-bot-workspace.log`, `/tmp/hermes-slack-bot-clippy.log`.


## Unmarked bot resolution

Unmarked users now resolve via authenticated users.info after self/ignored-channel
checks and workspace inference. Bot classification covers is_bot, is_workflow_bot
and profile.bot_id, preserving Python truthiness. The cache key includes team and
user. Cached false results include lookup failures, following Python's permissive
behavior. Successful insertion above 5,000 entries trims oldest entries to 2,500;
failure entries follow the source's untrimmed error branch. The cache currently
uses an ordered deque and has not been benchmarked at capacity.

An adapter without registered workspace credentials has no directory client and
does not issue user requests. Production registers workspaces before consuming
events. Declared bot markers bypass lookup. Resolved bots use the normal modes;
messages lacking client_msg_id also require the source's primary-bot mention gate.
User display-name caching and the full surrounding channel/mention policy remain.

The inline HTTP test verifies workspace-specific Authorization headers, bot/human
classification for the same ID in different teams, cache reuse, workflow/profile
markers, cached lookup failure, and message admission under none and mentions.

Validation: 1,264 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-user-workspace.log`, `/tmp/hermes-slack-user-clippy.log`.


## Filtered Block Kit UI payload

The new cohesive slack_blocks.rs module ports the source's serialized UI view.
It excludes rich_text blocks, preserves allowed scalar/recursive fields in input
order, drops empty recursive values and applies the same Unicode character limit.
Small limits retain Python's negative-slice behavior. URLs and button values are
not part of the recursive field allowlist; allowed scalar fields are copied as
Python does, so this is a field filter, not a general secret scanner.

Normal message parsing appends this view outside slash commands. Block-only
section/button events now have model-visible text instead of being dropped.
Rich-text-only blocks remain for the separate authored-text renderer to port.
Bang-command rewriting, text normalization/deduplication and legacy attachment
rendering are still unfinished.

35 fixtures execute the actual Python serializer, including Unicode, empty values,
mixed rich/UI blocks and truncation boundaries. An inline async adapter test covers
block-only delivery to model_content, excluded fields, slash argument preservation
and lack of duplicate rich-text payload. These are local tests without a live
Slack account or model provider.

Validation: 1,266 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-block-workspace.log`, `/tmp/hermes-slack-block-clippy.log`.


## Authored rich text and deduplication

slack_blocks.rs now renders authored sections, nested quotes, lists, preformatted
code, links, permalinks and inline Slack entities. The live non-command parser
adds only content missing from flat text, before appending the filtered UI view.
It resolves the workspace bot identity for mention normalization. This supersedes
the earlier notes that authored rich text is not yet wired.

Normalization follows the source order: Slack HTML entities, links/dates,
permalinks, labeled mentions, bot mention removal, code and nested styles, then
whitespace collapse. Ordinary text uses substring deduplication. Preformatted
content compares complete normalized fenced blocks, preserving a short snippet
when it only appears inside a larger code block.

396 cases execute the actual Python renderer/normalizer/merge helpers, including
quotes/lists, Slack entities, escaped permalink queries, date labels,
styles and code boundaries. The inline adapter test verifies mirrored flat text is
not duplicated, forwarded text survives, and block-only authored content reaches
message preparation. Legacy attachments and bang-command rewriting remain gaps.

Validation: 1,267 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-rich-workspace.log`, `/tmp/hermes-slack-rich-clippy.log`.


## Live attachment previews

The live parser now appends legacy attachment previews after Block Kit text,
matching the inbound Python branch rather than the separate thread-history helper.
It preserves title/link, preview text or fallback, and footer, skips is_msg_unfurl,
and limits the stripped body to 500 Unicode characters (497 plus ellipsis).
Only the full rendered section in the original text triggers deduplication, before
footer insertion. Repeated entries in the same attachment array are retained as
in Python. Live previews also follow Python's attachment behavior on commands.

48 cases execute the actual source branch, excluding only its diagnostic logging.
They cover URL/title/body combinations, original-URL retention, footer handling,
message unfurls, duplicate entries, whitespace and ASCII/Unicode truncation. The
existing adapter test now covers preview-only messages and visible preview content
when the URL is already in flat text. Structured fields/nested blocks used by the
separate thread-history renderer remain unfinished.

Validation: 1,268 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-attachment-workspace.log`, `/tmp/hermes-slack-attachment-clippy.log`.

The first attachment workspace run hit an unrelated conversion test failure:
audio_process::conversion_workspace_is_private_and_removed_on_success_or_error
could not read its subprocess capture file. The focused rerun passed, followed by
a passing workspace rerun in `/tmp/hermes-slack-attachment-recheck.log`. The initial
failure log is preserved above; the cause was not established and no conversion
code was changed. Focused evidence: `/tmp/hermes-audio-conversion-recheck.log`.


## Bang command recognition

command_catalog.rs consumes the generated Python registry metadata (101 command
definitions) without importing Python at runtime. Gateway recognition includes
aliases and config-gated commands while excluding CLI-only entries, matching the
source set. This is recognition metadata, not new command handlers. Dynamic plugin
commands remain excluded until the plugin command runtime exists.

Slack rewrites a recognized leading ! command before Block Kit merging. Only a
successful rewrite removes leading whitespace. Name comparison is lowercased and
ignores the @bot suffix while the rewritten text retains token spelling and all
arguments. Unknown exclamations and slash-containing command names remain intact.

261 fixtures execute the actual Python rewrite and registry predicate, with only
the lazy plugin enumerator replaced by an empty list. The inline adapter test
checks !help becomes /help, avoids duplicated mirrored blocks and resolves through
the existing native help handler. The catalog and oracle are regenerated with
rust/tools/gen_command_catalog.py; media fixtures use their existing generator.

Validation: 1,269 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, both fixture generators and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-bang-workspace.log`, `/tmp/hermes-slack-bang-clippy.log`.


## Redirected media downloads

Production media downloads now follow 301/302/303/307/308 responses with Location,
resolve relative targets, validate HTTP(S)/host/no-userinfo syntax, and construct a
DNS-pinned public-address client for each hop. The initial URL still requires the
existing HTTPS Slack CDN allowlist. A public off-origin signed-media destination
is allowed but does not receive Authorization; credentials stay removed even if
the chain returns to the initial origin. Final status/body/cache checks are shared
with the existing audio and video paths. Redirect bodies are not cached.

The origin rule follows the pinned HTTPX 0.28.1
[redirect header implementation](https://github.com/encode/httpx/blob/0.28.1/httpx/_client.py#L505).
Its HTTP-to-HTTPS credential exception is unreachable for our authenticated chain:
the initial URL is HTTPS and any downgrade has already removed authorization.
The 20-hop cap follows its
[default configuration](https://github.com/encode/httpx/blob/0.28.1/httpx/_config.py#L232).

A local HTTP test runs the real redirect loop with fixture DNS overrides. It proves
same-origin credentials, cross-origin removal, no restoration on return, cache
bytes, private-target rejection via the production resolver, userinfo rejection
before a request, and the redirect limit. This is not a live Slack/TLS test.
Cookies, retries, unusual HTTPX Location normalization and complete URL-safety
configuration parity remain unfinished; the native private-address rules remain
those documented earlier.

Validation: 1,270 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-redirect-workspace.log`, `/tmp/hermes-slack-redirect-clippy.log`.


## Media retries

Media downloads now retry the complete redirect/download/cache operation for
reqwest request/read timeouts and HTTP status failures from 429 upward, matching
the Python branches. There are three total attempts and 1.5/3-second delays.
Authorization selection restarts from the original URL on each attempt. Status
validation now precedes HTML rejection, so an HTML-formatted retryable HTTP error
is classified correctly. Non-timeout transport errors, access failures below 429,
HTML success responses, redirect validation errors and local cache failures do not
retry. DNS preflight timeout is not classified as a request timeout.

An inline local HTTP test verifies 503 then 429 then successful cached audio,
real backoff duration, one-request 403/HTML failure, and timeout exhaustion at three
attempts. Cookies remain a separate missing part of the Python HTTP client path.

Validation: 1,271 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-retry-workspace.log`, `/tmp/hermes-slack-retry-clippy.log`.


## Media cookie lifetime

The production download owns a Reqwest Jar for the entire attachment operation,
including retries. The redirect loop selects Cookie headers by destination URL
and stores all Set-Cookie headers before status/redirect processing. Thus redirects
and retryable error responses can establish cookies. A new attachment starts with
a fresh jar. This does not enable cookies on general Slack API clients or other
platform adapters; cookies are attached explicitly within this media loop.

The installed Reqwest 0.12.28 cookie.rs implementation was inspected locally for
CookieStore::cookies/set_cookies and Jar behavior after online source fetches were
unavailable. The existing reqwest cookies feature is enabled; Cargo.lock adds its
cookie dependencies without changing existing package versions.

An inline local HTTP test verifies same-host redirect cookies, path matching,
Secure withholding on HTTP, host-only exclusion across origins, fresh-jar isolation
and retention from a 503 through the next retry. It uses fixture DNS overrides,
not live Slack/TLS. Full Python CookieJar equivalence for expiry, public suffixes,
malformed attributes and cookie ordering remains unproven.

Validation: 1,272 workspace tests passed, two ignored. Clippy with warnings denied,
formatting and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-cookie-workspace.log`, `/tmp/hermes-slack-cookie-clippy.log`.


## Channel and DM controls

Missing/falsy channel_type on a D-prefixed channel now implies im. Both im and
mpim use DM source scope, while only im gets the channel-allowlist exemption.
Disable-DMs suppresses both. Other shared surfaces enforce the nonempty allowed
channel set once authenticated bot identity is known, following Python's gate.
Allowed channels do not interpret an asterisk as a wildcard.

Configuration resolves extras before legacy environment/YAML bridge behavior.
Non-string/list extras are ignored for allowed_channels, whereas disable_dms uses
Python truthiness for non-string extras and explicit true strings otherwise.
Normal-message controls run before users.info and file metadata/download work;
lifecycle file_shared still needs its initial metadata resolution as in Python.

40 fixtures execute the source policy methods. Inline async tests verify inferred
DM scope, mpim scope with shared-surface restrictions, both DM-disable branches and
allowed-channel messages. The HTTP fixture checks that blocked normal messages
cause no user/file requests. Full mention gates and early sender authorization
remain unfinished, as does thread/session reply routing.

Validation: 1,273 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-channel-policy-workspace.log`, `/tmp/hermes-slack-channel-policy-clippy.log`.


## Thread identity and outbound replies

Message now carries optional original message_id and selected thread_id, preserved
by Dispatcher reply construction. The shared native session-key helper includes
thread identity for history, leases and model cache scope; unthreaded keys retain
their earlier format. New thread keys are an intermediate native format, not full
Python session-key parity or an automatic migration of prior unthreaded history.

Slack selects DM/MPIM and channel roots using source defaults and extras settings.
Legacy shared-DM history is separate from reply placement. With reply_in_thread
false, real thread replies stay threaded while synthetic roots send flat. Wire
requests include thread_ts only when selected. Assistant-thread metadata lookup,
thread backfill, reaction handoff and full session resolution remain unfinished.

144 cases execute the source thread-selection branch and reply resolver. An inline
SQLite test verifies separate roots and shared history for a child in its root.
A local HTTP test inspects threaded and flat chat.postMessage bodies. Dispatcher
assertions verify both IDs survive reply construction. These are local checks,
not a live Slack/thread demonstration.

Tracing final MessageEvent construction also corrected command behavior: enriched
attachments are discarded for command text in Python. Native slash/bang command
text now likewise remains unchanged, superseding the earlier live-attachment note
about commands. A regression assertion covers /help with a preview attachment.

Validation: 1,275 workspace tests passed, two ignored. Clippy with warnings denied,
formatting, fixture regeneration and whitespace checks pass. Temporary logs:
`/tmp/hermes-slack-thread-workspace.log`, `/tmp/hermes-slack-thread-clippy.log`.

## Commands addressed to the bot

Ported the post-mention command probe from the source handler: strip the workspace
bot's exact mention, then recognize slash commands and known bang commands again.
The command is recovered from canonical event text, so rich blocks and preview
attachments cannot become command arguments. Ordinary messages retain enrichment;
unknown bang phrases remain ordinary text. Regex mention patterns are still absent,
as are the full channel wake policy and mentioned-thread state.

The existing inline adapter test now covers leading whitespace, slash arguments,
bang commands with a bot suffix, unknown bang phrases and another user's mention.
It calls prepare_message and verifies downstream slash recognition. All calls
use synthetic identities and no external Slack service.

The root data/ ignore rule hid the compile-time commands.json input. A narrow
exception now exposes that catalog while keeping other data files ignored.
Validation: 1,275 workspace tests passed, two ignored. Fixture regeneration,
formatting and whitespace checks pass. Logs for this slice are
`/tmp/hermes-slack-addressed-test.log` and
`/tmp/hermes-slack-addressed-workspace.log`.

## Addressed-user policy and wake-word exceptions

The native adapter now applies ignore_other_user_mentions to shared channels and
MPIM before file hydration. A leading other-user mention suppresses the turn
unless a self mention or configured regex wake word exempts it. One-to-one DMs
bypass this gate. Both workspace and primary bot IDs count as self for this test.
The predicate runs even for internal force-process events, matching its position
before that exception in the source. The full require/strict/thread mention gate,
thread wake cache, backfill and user authorization remain separate unfinished work.

Routing text now uses the source's flat-text plus authored block-mention algorithm.
Quoted mentions are excluded; duplicate authored tokens are retained when absent
from flat text. Pipe-labeled bot mentions exempt addressed messages without
changing the source's separate exact-token mention check.

Wake patterns accept extras strings/lists and legacy JSON or comma/newline text,
using the shared case-insensitive fancy-regex compiler. Invalid patterns are
skipped. The resolved-bot gate accepts a pattern match; the earlier declared-bot
gate still requires a literal primary mention. Pattern-triggered turns also use
the post-mention command probe. This supersedes the preceding note saying regex
patterns were absent. Arbitrary Python regex syntax and Unicode equivalence are
not proven by these tests.

slack-addressed-goldens.json contains 48 message cases and 44 setting cases,
generated by executing the actual Python methods. The inline adapter test covers
rejection before file handling, MPIM versus DM, labeled self mentions, wake-word
acceptance, ordinary references to others, and declared/resolved bot differences.
No external service or real credential is used. Full workspace validation passed
1,276 tests, with two ignored. Fixture regeneration, formatting and diff checks
pass. Logs: /tmp/hermes-slack-mention-workspace.log and
/tmp/hermes-slack-mention-clippy.log.

## Strict and thread mention rejection

Ported the unconditional rejection portion of the channel gate before file
hydration. Strict mode rejects unmentioned messages unless the channel is free
response (or require_mention is false). A require_mention_channels entry overrides
that free response. thread_require_mention rejects genuine unmentioned replies
even in a free channel. Exact/pattern mentions and forced internal turns bypass
this predicate; one-to-one DMs bypass the channel gate. The earlier addressed-user
rejection still takes precedence over forced turns.

The shared mention-flag parser preserves extras truthiness, explicit-false parsing
for require_mention, and the YAML/environment bridge. No whitespace stripping is
introduced for these boolean strings. Lists reuse the existing channel parser;
free_response_channels additionally accepts scalar extras like the source.

The oracle extracts the source force/free/strict/thread branch, replacing bare
returns with rejection markers and stopping before the asynchronous wake check.
256 combinations match Rust. This does not test or implement that wake check:
default require-mention behavior still needs bot-root, mentioned-thread, active
session and fetched-parent evidence before the native path has full parity.

Inline adapter assertions cover rejection before incomplete file metadata,
free top-level messages, per-channel overrides, explicit mentions, force events,
and DM/MPIM differences. The workspace passed 1,277 tests, two ignored. Fixture
regeneration, formatting and whitespace checks pass. Temporary logs:
/tmp/hermes-slack-strict-test.log, /tmp/hermes-slack-strict-workspace.log,
and /tmp/hermes-slack-strict-clippy.log.

## In-memory thread markers

Mentioned-thread recording now runs after existing gates, before slow enrichment,
and uses the selected session root. Strict/thread-required modes skip recording.
Successful outbound sends record both the returned message timestamp and the
outgoing root. Workspace IDs scope both sets; empty workspace IDs retain a
separate legacy representation. The default cap is 5,000 per set. Mention
records discard half the cap on overflow; sent records trim down to half the cap.

Eviction compares normalized integer seconds, six-digit fraction components, then
raw timestamp text, as in Python. It accepts oversized integers without machine
integer saturation. For equal timestamps across workspaces, Rust retains a stable
workspace tie order; Python's set tie order is unspecified.

21 transitions execute the actual Python marker/eviction methods. Inline tests
verify same timestamps remain separate across workspaces, strict mode and ignored
channels do not add mention memory, and the local HTTP send response records its
timestamp/root. The sets are recorded but not yet consumed by the unfinished
wake resolver. Active-session reset checks and API-derived thread parent evidence
must still be integrated before default require-mention gating is complete.

Workspace validation: 1,278 passed, two ignored. Fixture regeneration, formatting
and whitespace checks pass. Logs: /tmp/hermes-slack-markers-workspace.log and
/tmp/hermes-slack-markers-clippy.log.

## Thread-history rendering and cold parent fetch

Ported the source's readable history renderer into slack_blocks.rs. It includes
structured attachment fields, nested rich text, fallback-only attachments,
section/header/context text, ordered unique HTTP(S) block URLs, and sanitized
file-name markers. URL comparison decodes Slack entities once. Live-turn
attachment formatting remains separate, as it is in Python.

480 cases execute the actual source renderer and its helpers. The native cold
parent-fetch method requests conversations.replies with channel/ts, limit=1 and
inclusive=true using the workspace token. It requires a matching first root
message and returns empty text on HTTP/API failures or missing/mismatched roots.
A local HTTP test inspects the request, token, rendered blocks/files and failures.
No real Slack credentials or external service are involved.

Source quirk preserved: _fetch_thread_parent_text(strip_bot_mention=False) still
passes bot_uid to _render_message_text on a cold miss, which strips exact bot
mentions. A warm raw-message cache can return unstripped text. Do not replace
this difference with an assumed uniform behavior when porting the cache.

The cold-fetch helper is not yet wired into inbound wake routing. The shared
thread-context cache, fetched-root authorship and reset-aware active-session
checks remain unported. Existing native history presence alone is not equivalent
to Python _has_active_session_for_thread: reset policies can invalidate an entry.

Validation: 1,280 workspace tests passed, two ignored. Fixture regeneration,
formatting and diff checks pass. Logs: /tmp/hermes-slack-history-render.log,
/tmp/hermes-slack-parent-workspace.log and /tmp/hermes-slack-parent-clippy.log.

## Thread block formatting

Ported the source _format_thread_context output rules into the existing text
module. Formatting consumes resolved display names and authorization results;
it does not call APIs or alter stored conversation history. It excludes the
current triggering message, filters messages at/before a string watermark,
retains parent text separately even when the parent is filtered from the delta,
and distinguishes workspace-owned assistant replies from other bot posts.

Human/unverified names and bodies use the existing inline neutralizer. Prior
self-bot reply bodies remain verbatim, matching the source branch. Both header
variants and their trust wording are preserved source literals. A bot post
without a user ID resolves as 'unknown', including the source's unreachable
username fallback behavior.

108 cases run the actual Python async formatter with supplied name/authorization
results and the real source renderer, bot classifier and neutralizer. They cover
watermarks, current-message exclusion, workspace identity, prior assistant turns,
authorization true/false/unknown, and multiline attacker-controlled names/text.
The Rust formatter matches both returned values. This remains a dependency for
the pending cache/fetch integration; name-resolution HTTP and the runner's auth
callback still need connection at the adapter boundary.

Validation: 1,281 workspace tests passed, two ignored. Fixture regeneration,
formatting and whitespace checks pass. Logs: /tmp/hermes-slack-thread-format.log,
/tmp/hermes-slack-thread-format-workspace.log and
/tmp/hermes-slack-thread-format-clippy.log.

## Adapter thread fetch/cache and display names

The thread fetch now calls the formatter with names resolved through users.info
and an explicit caller-supplied authorization predicate. Name lookup is scoped by
workspace, falls back through display/real/handle/ID fields, caches failures as the
raw ID, and seeds bot classification. The existing bot resolver now also seeds
names, preventing a second API call. Source-specific channel-less lookups use the
primary token while retaining the supplied workspace cache key.

The adapter cache stores full formatted content, parent text/author, raw messages
and fetch-start time. Fresh no-watermark requests reuse the original content;
watermarked requests reformat raw messages. Forced refresh bypasses the cache.
Entries expire after 60 seconds; above 2,500 entries only stale entries are pruned,
matching the source's soft cap. conversations.replies uses limit+1 and inclusive,
with up to three attempts and 1s/2s delays for rate-limited HTTP/API failures.
Failures and empty message arrays do not replace a prior entry.

The parent helper now consults this cache. It returns stored rendered parent text
when stripping is requested, and raw parent text otherwise. A missing raw parent
falls through to the existing cold lookup. This implements the warm/cold mention
preservation distinction recorded above.

Local HTTP tests exercise workspace tokens, name/bot cache reuse in both
directions, fallback names, cached failures, inferred workspace and primary-client
routing; full-context cache hits, delta formatting, forced refresh, expiry,
returned parent text and successful recovery after two 429 responses. Synthetic
credentials only. The fetch chain is now connected internally, but inbound wake
routing still does not invoke it: reset-aware session checks and the runner's
real authorization callback remain necessary before full integration.

Validation: 1,283 workspace tests passed, two ignored. Fixture regeneration,
formatting and whitespace checks pass. Logs: /tmp/hermes-slack-names-test.log,
/tmp/hermes-slack-thread-cache-test.log,
/tmp/hermes-slack-thread-fetch-workspace.log and
/tmp/hermes-slack-thread-fetch-clippy.log.

## Thread wake resolver, resumed after rest

Connected the five source wake checks: sent-message markers for genuine replies,
mentioned-thread memory, a caller-supplied reset-aware active-session probe,
bot-authored roots, and fetched raw parent mentions. The empty-thread and
non-reply short circuits match Python. No history-count substitute is used for
session activity. Existing thread fetch, name resolution, formatting and parent
cache now provide the resolver's API evidence.

Preserved two source behaviors that matter during subsequent integration:
root-authorship reads the first channel/thread cache-prefix match without a TTL
or exact workspace-key check; parent-mention wake registers a legacy unscoped
marker because that source call omits team_id. Neither is silently corrected in
this port. Tests cover legacy marker reuse across a different workspace.

96 cases execute the actual Python wake method and compare decisions, active
probe calls and newly registered markers. A local HTTP test covers uncached
bot-owned roots, parent mentions, ordinary roots, missing roots, memory reuse
and active-session short circuiting. No external Slack service is contacted.

Validation: 1,289 workspace tests passed, two ignored. Fixture regeneration,
formatting and diff checks pass. Logs: /tmp/hermes-slack-wake-test.log,
/tmp/hermes-slack-wake-workspace.log and /tmp/hermes-slack-wake-clippy.log.
The resolver is still ahead of inbound integration: SessionStore entry/key/reset
and authorization hooks must be connected before the adapter has full default
require-mention behavior.

## Session-store lookup keys and profile resolution

Added the Slack thread-key helper using the shared SessionSource/build_session_key
implementation and GatewayConfig isolation settings. IM/MPIM are DM sources;
channels are group sources. Missing store config returns no key. Profile selection
follows BasePlatformAdapter: stamped source profile, credential owner, then store
resolver; non-string/blank candidates are ignored, resolver errors yield no
profile, and valid spelling is preserved rather than trimmed.

288 cases execute the actual Python Slack wrapper and shared key builder. The
first run found a pre-existing shared-builder mismatch: Some(empty) thread IDs
appended an extra colon, and empty alternate IDs masked the real user ID. Fixed
truthiness/fallback handling in the shared builder, including prospective-thread
fallback and the sibling shared-session predicate. Inline regressions cover the
empty DM, group and prospective-thread cases.

Validation: 1,292 workspace tests passed, two ignored. Fixture regeneration,
formatting and whitespace checks pass. Logs: /tmp/hermes-slack-thread-key-test.log
(initial failure), /tmp/hermes-thread-key-workspace.log and
/tmp/hermes-thread-key-clippy.log. SessionStore entry loading and inbound routing
integration remain pending; this does not migrate existing native history keys.
