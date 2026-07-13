# CLAUDE.md

Guidance for Claude Code working in this repository.

## Project Overview

**TetherMoon** — a safe Rust FFI wrapper for the Sony Camera Remote SDK v2.01.00 plus a
browser-based tethering server. Target: Sony A7C (ILCE-7C), macOS (Apple Silicon), USB.
Free & open source (MIT). Shipped v1.0; post-1.0 added an astro toolset (night mode,
Star AF, focus meter, bracketing, live stacking, shooting plan) — see `STATUS.md`.

## Workspace Map

| Crate / dir | Role |
|---|---|
| `crsdk` (root, `src/`) | Safe Rust FFI wrapper lib (session/enumerate/connection/liveview/shutter/properties/callback). `capability` = runtime probe (what a body exposes); `body::BodyProfile::for_model` = static per-body quirks (AF calib, BULB encoding) with safe degrade for unknown bodies |
| `crsdk_server/` | axum/tokio HTTP·SSE·MJPEG server + web UI. All SDK calls via `spawn_blocking`. Modules by domain: `state`(AppState/guards) · `lifecycle`(connect/shutdown/single-instance) · `props` · `swaf`(SW-AF/Star AF/focus meter) · `afpoint`(HW AF+calib) · `capture`(shutter/bulb/interval/bracket/multi-exp) · `stack` · `storage` · `stream`(lv producer/SSE); `main.rs` = router+startup only |
| `stacker/` | Star-alignment + frame-stacking engine. Pure math, no camera dep, unit-tested |
| `detector/` | RT-DETR CoreML object detection (tracking AF). Optional |
| `crsdk_server/web/` | `index.html`(console) · `plan.html` · `stack.html` · `remote.html` + `app.js`, `styles.css` |
| `wrapper/` | C shim (`wrapper.cpp/h`) bridging the C++ SDK to pure C |
| `scripts/` | `make_app.sh` → `make_dmg.sh` (macOS packaging), `package-win.ps1`, `test.sh` |
| `gallery/cards/` | SNS/OG card HTML templates + `render.sh` (headless Chrome regenerate) |

Feature flags on `crsdk_server`: `--features raw` (true ARW decode via **rawler**),
`--features detector` (CoreML tracking AF). Default build includes neither.

## Build, Run, Test

```bash
# The SDK dylibs must be on DYLD_LIBRARY_PATH for build AND test AND run:
export DYLD_LIBRARY_PATH="$DYLD_LIBRARY_PATH:$(pwd)/CrSDK_v2.01.00_20260203a_Mac/RemoteCli/external/crsdk"

cargo build -p crsdk_server            # default server build
cargo build -p crsdk_server --features raw
cargo test  -p stacker                 # engine tests — no SDK/camera needed
cargo test  -p crsdk_server            # needs DYLD path (links the SDK)
cargo run   -p crsdk_server            # http://localhost:8080/web/index.html
```

Packaging: `./scripts/make_app.sh` then `./scripts/make_dmg.sh` → `dist/TetherMoon-v<ver>.dmg`
(version read from `crsdk_server/Cargo.toml`). App is adhoc-signed — first launch needs
`xattr -dr com.apple.quarantine`.

## Architecture (short)

```
Sony C++ SDK → wrapper/wrapper.cpp (pure-C shim, opaque void* handles)
  → build.rs (cc + bindgen) → src/ffi.rs → safe modules in src/
  → crsdk_server (axum; camera in Arc<Mutex<Option<CameraCell>>>, SDK calls on spawn_blocking)
  → web/ single-page UI (2s polling + SSE /events + MJPEG /lv)
```

- **RAII everywhere**: `SdkSession`/`CameraConnection`/etc. clean up in `Drop`. Never call
  release functions manually. Camera teardown goes through `release_camera()` (take + drop
  on spawn_blocking) — never drop the camera while holding the lock on an async thread.
- **Single live-view producer** fans out frames via `broadcast`; consumers subscribe.
  Run-guards (`RunGuard`, `LvGuard`) release single-run flags on every exit path.
- macOS `ptpcamerad` interferes with USB camera access; the server kills/suppresses it.
  Expected behavior, not a bug.

## Gotchas (learned the hard way)

- **rawloader does NOT support the A7C** ("Couldn't find camera SONY ILCE-7C").
  Use **rawler** (dnglab) — it decodes ILCE-7C ARW. Already wired under `--features raw`.
- **Never `pkill -f crsdk_server`** from a tool shell — the pattern matches your own shell
  command line and kills it (exit 144). Kill by pid: `kill $(lsof -tiTCP:8080 -sTCP:LISTEN)`.
- The server enforces a **single instance on :8080** (kills predecessors on launch); when
  testing manually, stop the old instance first.
- `cargo test -p crsdk_server` still needs `DYLD_LIBRARY_PATH` (test binary links the SDK).
- **ARW previews** (`/api/last_image`) work by extracting the embedded full-res JPEG
  (`extract_embedded_jpeg`); candidates are validated by actually parsing the JPEG header
  (bogus FF D8…FF D9 runs occur in raw sensor data).
- `GetValueSize()`-family SDK calls return **byte counts, not element counts** — divide by
  element size or you heap-over-read (this bug happened twice).
- `STATUS.md` is **gitignored** (local dev log). `ARCHITECTURE.md` and READMEs are tracked.
- Web pages use absolute asset paths (`/web/...`) — they only render via the server, not file://.

## Verification Patterns (no camera required)

Most changes here were verified without hardware — prefer these before claiming done:

- **Engine logic** → unit tests in `stacker` (synthetic star fields: inject points, shift/rotate,
  assert alignment/rejection).
- **Server endpoints** → run the server, `curl` the API (e.g. `/api/stack/folder` with a dir of
  generated JPEGs; check `status` counts and fetch `preview`). Python/PIL generates test frames.
- **Web UI** → headless Chrome screenshot: `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
  --headless=new --screenshot=... "http://localhost:8080/web/..."` and inspect the PNG.
- Hardware-only claims (real capture, focus on a real star) — say so explicitly; don't fake them.

## FFI Conventions

- SDK objects cross the boundary as opaque `*mut c_void` — never dereference in Rust.
- C wrapper functions are prefix-less descriptive names (`sdk_init`, `camera_connect`, …).
- The authoritative `bindings.rs` is generated into `OUT_DIR` by `build.rs`; the repo-root
  copy is a committed snapshot.
- `build.rs` expects the SDK at `./CrSDK_v2.01.00_20260203a_Mac/` (not committed; from Sony).

## Docs

- `STATUS.md` — feature log incl. post-1.0 astro tools (local only)
- `ARCHITECTURE.md` — structure detail; `docs_md/`, `html_md/` — converted Sony SDK docs
- `.claude/skills/crsdk-verify` — repo skill for verification flow

## Approach

- Think before acting; read existing code first. Match its style (comments are Korean — keep that).
- Surgical edits over rewrites. Every changed line traces to the request.
- Verify with one of the patterns above before declaring done. Report failures honestly.
- User instructions always override this file.
