# Native image verification, 2026-09-05

Native content construction now reads actual files, checks the read policy,
sniffs their MIME, preserves accepted formats as base64, and converts supported
non-native formats to PNG. File safety and MIME inference have concrete callers
in this pipeline. The pipeline still needs a live dispatcher/agent consumer.

| Contract | Evidence |
| --- | --- |
| Read-denial ordering and path resolution | 69 real-filesystem Python cases, including profile/root credentials, missing tails, symlink parents, cycles and unknown named users |
| Byte sniffing | 23 signatures executed by the Python source |
| Native parts, skipped files, URLs, captions | 20 cases executed through Python's real file loader, real read guard, and Pillow |
| Transcoding | MIME, dimensions and RGBA pixels compared for BMP, RGBA TIFF, integer/float TIFF and WebP with a narrowed accepted-format set |
| MIME lookup | 78 CPython cases for built-ins, supplied overlays, encoding aliases, case, hidden names and ordinary URLs |
| Document fallback | 56 cases executing the actual document MIME branch from gateway/run.py |

Pass-through images compare exact bytes. Transcoded PNGs compare pixels and
dimensions because different encoders can produce different compressed streams
for the same picture. No resizing or proactive size threshold was added.

## Corrections during validation

- The helper used Python 3.14's non-strict symlink-cycle behavior. The supported
  3.11/3.12 pathlib reference raises instead, then the outer best-effort guard
  swallows that resolution error. The Rust port now preserves both boundaries.
- Unknown `~user` expansion raises in pathlib rather than leaving a literal
  path. Named-user lookup now uses the reentrant libc API with owned buffers,
  avoiding static passwd storage shared between threads.
- Python's extra ASCII whitespace separators must be stripped from captions
  and URL strings. The initial native content fixture exposed the mismatch.
- The generic image decoder skipped signed integer TIFF and scales 16-bit
  samples differently from Pillow. A direct TIFF path now clips numeric
  grayscale samples as Pillow's RGBA conversion does; integer, 16-bit and float
  inputs are compared against the reference.
- A static MIME crate would omit host mappings. It was replaced with CPython
  default tables and the same ordered mime.types overlays. This resolver also
  feeds the document fallback helper.
- The full suite exposed a pre-existing config golden test race: construction
  and expected sessions_dir could observe different homes. The golden helper
  now holds the shared environment lock, and media tests acquire that lock
  before changing either HOME variable.

## Remaining limits

HEIC/AVIF byte signatures are recognized, but this build lacks their optional
decoders. These attachments currently skip when conversion is required; this
is not complete decoder parity with Pillow installations supporting them.
Codec modes beyond the tested samples still need comparison. MIME URL handling
covers ordinary schemes; malformed authorities and unusual control-character
normalization still need strict urllib comparison. The read guard targets
POSIX and UTF-8 paths; non-UTF-8 home names and non-POSIX behavior remain work.
Live capability probes, provider-scoped accepted formats, rich adapter events,
and multimodal agent transport are not proven by these tests.

## Reproduce

```bash
mise exec python@3.12.13 -- python rust/tools/gen_file_read_safety_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_mime_goldens.py --check
mise exec python@3.12.13 -- python rust/tools/gen_media_context_goldens.py --check
/home/eins0fx/.hermes/hermes-agent/venv/bin/python rust/tools/gen_native_image_goldens.py --check
cargo test --manifest-path rust/Cargo.toml --workspace
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo fmt --manifest-path rust/Cargo.toml --all --check
```

The image oracle uses the shared Python 3.11.15 venv with Pillow 12.3.0. The
other generators use mise Python 3.12.13. Current workspace: 993 passed, one
existing ignored test. Logs: `takeover-native-workspace-tests.log`,
`takeover-native-goldens-tests.log`, and `takeover-native-clippy.log`.

Dependency APIs checked against primary documentation:
[image 0.25.10](https://docs.rs/image/0.25.10/image/),
[TIFF decoding](https://docs.rs/tiff/0.11.3/tiff/decoder/struct.Decoder.html),
[base64 0.22.1](https://docs.rs/base64/0.22.1/base64/),
[Pillow Image](https://pillow.readthedocs.io/en/stable/reference/Image.html), and
[getpwnam_r](https://man7.org/linux/man-pages/man3/getpwnam.3.html).
The MIME defaults are data from CPython's PSF-licensed mimetypes standard module.
