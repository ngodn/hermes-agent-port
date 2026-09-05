# Native transcription HTTP transport

`transcription_http.rs` implements the OpenAI-compatible multipart transport
used by `tools/transcription_tools.py::_transcribe_openai`. Resolved model,
provider label, endpoint, key, language, and prompt come from the caller.
`HttpTranscriptionBackend` connects this transport to the existing gateway
transcription enrichment interface, using native duration probing and delegating local fallback
and sandbox path mapping to the runner context.

The transport uses the reference 30-second request timeout with no automatic HTTP retries.
It uploads a real file, applies the existing read policy before opening it,
rejects symlink/non-file inputs, and checks the 25 MiB remote upload limit.
Whisper uses text responses; other models use JSON. Native OpenAI requests
correct Groq-only model names to the caller-resolved OpenAI default, while other
provider labels preserve their model. Nonempty language and prompt hints reach
the multipart request. `gpt-transcribe` sends `languages[]`, verified against
the installed OpenAI Python SDK with an HTTP mock transport.

The multipart endpoint and response formats were cross-checked with the
[OpenAI audio reference](https://platform.openai.com/docs/api-reference/audio/json-object?lang=csharp).
The repository Python implementation remains the porting specification.

Verification uses a local HTTP server and temporary files. Four model cases
run through the actual enrichment function and check upload bytes, auth,
multipart fields, response normalization, retained caption, and transcripts.
Secret-file denial causes no HTTP request. A real HTTP 401 produces a failed
transcription envelope. Fifteen source-executed Python cases cover plain text
and ASR language-wrapper cleanup.

This is a transport integration, not complete live gateway STT. The Dispatcher
still needs attachment download/preprocessing and runtime construction of this
backend. Provider/credential resolution, local model fallback,
SILK conversion remains pending. HTTP/OS error details
currently use native error text rather than Python SDK exception formatting.
Malformed JSON response coercion and Unicode-regex edge cases also need wider
comparison. Redirects are disabled rather than following uploads to another
endpoint.

Rejected-container recovery now matches the reference HTTP 400 keyword gate:
unsupported, corrupted, or invalid file. It discovers ffmpeg in the two reference
prefixes then PATH, encodes mono 16 kHz AAC at 32 kbps with faststart, and retries
exactly once. Conversion has a 120-second timeout, kills the child on cancellation,
and owns a private work directory until upload completion. Local process tests
verify directory permissions and cleanup after successful and failed encoding.
The explicit optional FFmpeg test produces a real M4A and exercises four HTTP
cases: successful retry, repeated rejection, unrelated 400, and 401. Encode
flags also match the Python constant and [FFmpeg documentation](https://www.ffmpeg.org/ffmpeg.html).

Workspace validation: 1,215 tests passed, two ignored by default. The optional
FFmpeg test was explicitly run and passed; the Python bridge test remains ignored.
Clippy with warnings denied, formatting, and fixture regeneration pass.

Native duration probing now follows the gateway path: PCM/PCM-extensible WAV
headers first, then a five-second ffprobe subprocess with the reference argument
shape. Failed probes return no duration. Formatting uses ties-to-even rounding,
negative clamping and the reference minute/hour layout. Sixteen Python format
cases and 36 CPython wave headers compare results; WAV seconds use exact IEEE
bits to avoid JSON parser rounding in the oracle. A real temporary WAV through
disabled-STT enrichment produces the expected 1:01 duration note.

Ogg/Opus now use `ogg_opus_duration.rs` before ffprobe, for .ogg, .opus and .oga
extensions. The parser reads Ogg page lacing, identifies the Opus stream,
validates and joins comment packets, and selects the final usable granule
position. It subtracts pre-skip and converts at the fixed 48 kHz rate described
in [RFC 7845](https://www.rfc-editor.org/rfc/rfc7845.html). CRC checking is absent
in both this duration path and the reference Mutagen loader.

43 synthetic files are loaded through Mutagen 1.47.0's full OggOpus API and
compared using exact floating-point bits. Cases cover input sample-rate
independence, header versions, pre-skip, missing/malformed tags, split comments,
multiplexed serials, missing EOS, incomplete packets and damaged tails. A gateway
probe integration test checks three extensions and minute formatting. The
reference package is isolated through uv; it is not a Rust runtime dependency.
Successful ffprobe duration conversion still needs a dedicated real-media
regression; current tests cover native WAV/Opus success and malformed-audio
fallback failure. Full decoder validation is outside header-only probing.

Prepared-file validation now checks source kind and the 13 supported audio
suffixes before the transport's upload-size check. The reusable validation
helper also supports Python's optional early size cap. 46 source-executed cases
use real files, sparse files at/above 25 MiB, a directory, a missing file and a
dangling symlink, with both size-cap modes. The HTTP integration test verifies
that an unsupported .txt input produces no upload. The existing secret read
policy still runs first. Platform-specific OS error details remain native.

STT language resolution now accepts the provider language, historical aliases,
global default and legacy environment value in Python order. Only nonblank
strings qualify. The transport's configuration builder applies this resolution
when no nonempty caller/hook override exists. 108 Python comparisons cover
precedence and invalid types; explicit override tests include whitespace-only
hook values, which Python preserves. The four HTTP model tests now obtain their
language from configuration and verify the resulting multipart field. Runtime
construction still needs to supply the active profile config and scoped env.

The OpenAI configuration constructor now resolves credentials from raw saved
provider intent before creating the HTTP client. 100 Python cases verify lazy
effect order, direct/config/local precedence, explicit selection errors and
legacy managed fallback. The existing four-model HTTP enrichment regression
uses this constructor and verifies that config credentials cause no external
credential lookup. 257 Python locality cases cover the separate STT endpoint
predicate. Profile-aware credential effects, managed authentication and full
dispatcher construction remain pending; see the credential-resolution plan.
