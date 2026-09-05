# Rust port evidence index

Read this before resuming, then [PORT.md](../PORT.md) for current progress.

## Settled findings

- The rewrite is gateway-first and incremental. Keep the Python reference tree
  intact and new Rust work under `rust/`.
- Compiling a module is not runtime integration. The newer full config loader
  and session registry still await their runner consumers.
- The existing AGY wrapper serializes invocations after the observed auth race.
  Preserve that gate; helper results still need independent source verification.
- Inbound media classification is pure; the enclosing Tier 2 pipeline also
  performs model/network calls and per-session mutations.

## Rejected paths

- Do not use the AGY API-server map as an authoritative enumeration. Prior
  source checks found fabricated names and line numbers. Use the extracted
  route table instead.
- Do not substitute another model for either requested helper silently.
- Do not merge helper code on the strength of its own tests or completion
  report. Python behavioral comparison caught earlier helper test mistakes.

## Artifacts

| Artifact | Takeaway |
| --- | --- |
| [tool-result-verification.md](tool-result-verification.md) | Native result construction, threat scanner corrections and HTTP replay proof |
| [tool-result-goldens.json](../tools/tool-result-goldens.json) | 62 source-executed result construction and wrapping cases |
| [threat-pattern-goldens.json](../tools/threat-pattern-goldens.json) | 129 scanner comparisons against Python |
| [threat-word-ranges.json](../tools/threat-word-ranges.json) | Generated Python 3.12 word-character ranges for regex compatibility |
| [tool-events-verification.md](tool-events-verification.md) | Native call-event correlation and the remaining tool-result constructor contract |
| [refusal-verification.md](refusal-verification.md) | Refusal-only payload selection, user event delivery and remaining response-normalization scope |
| [refusal-goldens.json](../tools/refusal-goldens.json) | 48 cases executed through the actual Python chat response normalizer |
| [tool-call-replay-verification.md](tool-call-replay-verification.md) | Native tool metadata preservation, model-sensitive projection and reasoning echo startup integration |
| [chat-message-goldens.json](../tools/chat-message-goldens.json) | 30 source-executed Chat Completions projection cases |
| [reasoning-replay-goldens.json](../tools/reasoning-replay-goldens.json) | 62 source-executed host, provider family and reasoning policy cases |
| [prompt-cache-verification.md](prompt-cache-verification.md) | Native cache integration, persisted conversation scopes, override order and dispatcher lease regression |
| [prompt-cache-goldens.json](../tools/prompt-cache-goldens.json) | 65 source-executed cache scope, bounding, static prefix and key projection cases |
| [gateway-map.md](gateway-map.md) | Original gateway dependency map; check against current source |
| [agent-invocation.md](agent-invocation.md) | Python agent boundary used for the strangler bridge |
| [session-db.md](session-db.md) | Session storage mapping |
| [run-py-map.md](run-py-map.md) | Runner tier plan, with verified structure and noted inaccuracies |
| [api-server-map.md](api-server-map.md) | Rejected enumeration, retained with a warning |
| [api-server-routes.md](api-server-routes.md) | Mechanically extracted route table with handler checks |
| [tier2-source-audit.md](tier2-source-audit.md) | Gemini audit of inbound classifiers, side effects, and next seams |
| [inbound-media-review.md](inbound-media-review.md) | Claude follow-up review of the classification port and oracle |
| [inbound-state-verification.md](inbound-state-verification.md) | Context notes, pending merge/STT, native images, parity fixes and proof boundaries |
| [pending-stt-review.md](pending-stt-review.md) | Gemini review; composite and queue-state gaps subsequently addressed |
| [tools/README.md](../tools/README.md) | Helper invocation, requested permissions, and dgnrt reference findings |
| [inbound-media-goldens.json](../tools/inbound-media-goldens.json) | Executed Python outputs for 217 cases |
| [media-context-goldens.json](../tools/media-context-goldens.json) | Placeholder and document/audio/video text outputs |
| [pending-stt-goldens.json](../tools/pending-stt-goldens.json) | 27 source-executed cache/echo/composite transitions |
| [pending-message-goldens.json](../tools/pending-message-goldens.json) | 166 source-executed pending-event merges |
| [cache-path-goldens.json](../tools/cache-path-goldens.json) | 224 cache mappings from real Python imports |
| [inbound-text-goldens.json](../tools/inbound-text-goldens.json) | 144 sender/reply cases and 30 normalization cases |
| [transcription-goldens.json](../tools/transcription-goldens.json) | 38 transcription scenarios with provider call ordering |
| [vision-goldens.json](../tools/vision-goldens.json) | 51 vision scenarios with sanitizer and provider call ordering |
| [image-routing-plan.md](image-routing-plan.md) | Verified next steps and corrections to Gemini's draft APIs |
| [image-routing-verification.md](image-routing-verification.md) | Configuration/session routing and real-filesystem reference extraction evidence |
| [image-routing-goldens.json](../tools/image-routing-goldens.json) | 392 configuration and routing cases |
| [session-image-routing-goldens.json](../tools/session-image-routing-goldens.json) | 28 session-aware wrapper cases |
| [image-reference-goldens.json](../tools/image-reference-goldens.json) | 46 reference extraction cases on real files |
| [native-image-verification.md](native-image-verification.md) | Real image I/O, read guard, MIME inference, corrections and remaining decoder gaps |
| [native-image-goldens.json](../tools/native-image-goldens.json) | Byte signatures and real-file/Pillow output comparisons |
| [file-read-safety-goldens.json](../tools/file-read-safety-goldens.json) | 69 POSIX read-guard cases |
| [mime-defaults.json](../tools/mime-defaults.json) | CPython MIME defaults consumed by the runtime resolver |
| [mime-goldens.json](../tools/mime-goldens.json) | 78 path and overlay inference cases |
| [structured-content-verification.md](structured-content-verification.md) | Prepared image parts through real HTTP, native streaming/tool requests, SQLite replay, and unsupported-backend rejection |
| [content-storage-goldens.json](../tools/content-storage-goldens.json) | 14 Python storage codec cases, compared as decoded values |
| [live-capability-plan.md](live-capability-plan.md) | Verified endpoint resolution, remaining HTTP/cache dependencies, and corrections to the helper audit |
| [inference-endpoint-goldens.json](../tools/inference-endpoint-goldens.json) | 1,164 Python URL/key resolution cases using explicit runtime values |
| [local-probe-verification.md](local-probe-verification.md) | Real HTTP server detection, Ollama show, cache tests, integration fixes and remaining limits |
| [local-probe-goldens.json](../tools/local-probe-goldens.json) | 42 Python request/response and normalization cases |
| [endpoint-locality-verification.md](endpoint-locality-verification.md) | Locality comparisons, zero-request remote gate, and URL/key-to-Ollama HTTP integration |
| [endpoint-locality-goldens.json](../tools/endpoint-locality-goldens.json) | 253 CPython 3.12.13 locality cases including Unicode and address boundaries |
| [provider-prefix-plan.md](provider-prefix-plan.md) | Registry/discovery contract and rejected manifest-name substitutions |
| [managed-capability-verification.md](managed-capability-verification.md) | Staged models, PID ownership, live props, catalog/projector fallback and remaining lifecycle work |
| [managed-capability-goldens.json](../tools/managed-capability-goldens.json) | 50 source-derived staging and capability cases |

| [managed-catalog-verification.md](managed-catalog-verification.md) | Packaged curated catalog loading, refresh, HTTP tests, and remaining compatibility limits |
| [managed-catalog-goldens.json](../tools/managed-catalog-goldens.json) | 43 source-executed catalog constructor cases |

| [cloud-catalog-verification.md](cloud-catalog-verification.md) | Cloud registry cache, real HTTP and concurrency evidence, remaining metadata integration |
| [cloud-catalog-goldens.json](../tools/cloud-catalog-goldens.json) | 60 Python cache state transitions with disk, ETag, and network trace comparisons |

| [cloud-metadata-verification.md](cloud-metadata-verification.md) | Capability/context lookup, overrides, HTTP integration, and canonical JSON ordering checks |
| [cloud-metadata-goldens.json](../tools/cloud-metadata-goldens.json) | 683 Python capability and context comparisons, including insertion-order ties |

| [provider-registry-verification.md](provider-registry-verification.md) | Registration identity, prefix recognition, and full live vision lookup stage ordering |
| [provider-registry-goldens.json](../tools/provider-registry-goldens.json) | Six registration transitions and 265 Python prefix comparisons |

| [provider-fetch-verification.md](provider-fetch-verification.md) | Native model-list hook, credential-safe redirect evidence, and remaining TLS/discovery work |
| [provider-fetch-goldens.json](../tools/provider-fetch-goldens.json) | 49 Python model-list selection/body cases and 12 hostnames |

| [provider-tls-verification.md](provider-tls-verification.md) | CA bundle precedence, trust replacement, public fetch and real HTTPS checks |

| [bundled-base-profiles-verification.md](bundled-base-profiles-verification.md) | Native bundled definitions, startup request headers, credential rotation, transport fallback and remaining discovery work |
| [bundled-base-profiles.json](../tools/bundled-base-profiles.json) | 17 complete profiles generated from 13 base-only Python modules |

| [upstage-verification.md](upstage-verification.md) | Native Upstage request hook, reasoning config and real startup HTTP evidence |
| [upstage-goldens.json](../tools/upstage-goldens.json) | 138 hook, 208 clamping and 215 config-resolution source cases |

| [nebius-verification.md](nebius-verification.md) | Native Nebius profile and request hook, source comparisons and startup HTTP evidence |
| [nebius-goldens.json](../tools/nebius-goldens.json) | 812 Python model-gate and reasoning projection cases |

| [output-cap-verification.md](output-cap-verification.md) | Gateway cap resolution, token parameter selection and profile defaults on native requests |
| [output-cap-goldens.json](../tools/output-cap-goldens.json) | 132 parameter and 364 gateway/init resolution comparisons |

| [request-merge-verification.md](request-merge-verification.md) | Vercel, custom-provider settings and final HTTP JSON merge evidence |
| [request-merge-goldens.json](../tools/request-merge-goldens.json) | 20 actual Hermes/SDK projection cases |
| [vercel-goldens.json](../tools/vercel-goldens.json) | 32 real Vercel hook cases |
| [custom-request-goldens.json](../tools/custom-request-goldens.json) | 24 custom-provider selector cases from Python |

| [gemini-thinking-verification.md](gemini-thinking-verification.md) | Pre-hook effort normalization and native Gemini output-headroom integration |
| [gemini-thinking-goldens.json](../tools/gemini-thinking-goldens.json) | 64 config/cap cases executed from Python |
| [wire-reasoning-goldens.json](../tools/wire-reasoning-goldens.json) | 51 pre-hook normalization comparisons |

Raw local helper outputs: `port-inbound-media.claude.json` and `.stderr`,
`inbound-media-review.claude.json` and `.stderr`, `tier2-source-audit.agy.log`.
Validation outputs: `takeover-tests.log` and `takeover-clippy.log`.
