# Developing the Aizome Wayland backend

This repository is a fork of tembo/voxl, based initially on
`b00f3b8b8c156b14e0c75abdfcf5cae9a14a865f`. Keep the server's H.264, WebSocket,
WebCodecs, authentication, recording and flow-control paths shared across capture
backends. The native Remote Desks viewer is future work; no frontend latency
advantage has been measured by this change.

The fork's release workflow is inhibited by an upstream-repository guard. Do not
publish the upstream `voxl-server` crate from this fork.

## Build

The native Linux dependencies are x264, libyuv, Opus, PulseAudio and libxkbcommon;
pkg-config, clang/libclang and esbuild are build tools. `shell.nix` supplies them.
Wayland connections use wayland-client's Rust backend; no subprocess capture or
X11 bridge is used.

```sh
nix-shell --arg withRust true
cargo build --locked
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings \
  -A clippy::manual-is-multiple-of -A clippy::manual-checked-ops \
  -A clippy::chunks-exact-to-as-chunks
```

For work alongside Sekka, use the root flake's locked Nixpkgs instead of a channel:

```sh
sekka_nixpkgs=$(nix eval --raw --impure --expr '(builtins.getFlake "/home/callen/work/sekka").inputs.nixpkgs.outPath')
nix-shell -I "nixpkgs=$sekka_nixpkgs" --arg withRust true
```

Keep Voxl as a separate checkout and Cargo workspace. A future Sekka package can
pin this fork. No compositor change, ISO rebuild, release enablement or installed
session modification is required for the backend.

The Clippy exceptions cover existing upstream code under Rust 1.95. The microphone
typed-array annotations also account for the newer TypeScript DOM declarations;
they do not change emitted JavaScript or media behavior.

## Interfaces and lifecycle

- `src/capture.rs` selects X11 or Wayland at runtime and supplies the existing
  video pipeline's interface. The original X11 implementation is in
  `src/x11_capture.rs`.
- `src/wayland/capture.rs` binds `ext-output-image-capture-source-v1` and
  `ext-image-copy-capture-v1`. One `wl_shm` XRGB8888 buffer is reused. Pixels are
  read only after `ready` and copied to a stable image before the next request.
  Damage is clipped, accumulated and translated to x264's unchanged-block hints.
  A quiet scene leaves one capture waiting in the compositor. The existing
  encoder pacing and acknowledgments control the downstream pipeline.
- Updated constraints rebuild capture/encoding and send fresh geometry/keyframes.
  A revoked session or removed output ends streaming. Revocation never silently
  creates a replacement session. Captures must have even dimensions for I420 and
  be upright as Aizome delivers them; unsupported sizes/formats/transforms fail
  explicitly rather than being misinterpreted. Buffers are capped at 256 MiB.
- `src/wayland/ipc.rs` implements only the small public Aizome IPC wire boundary
  used for output geometry and input. It uses `--aizome-socket`, `AIZOME_SOCKET`,
  or `$XDG_RUNTIME_DIR/aizome.$WAYLAND_DISPLAY.sock`. Input stays unprivileged;
  no root helper, uinput device or compositor privilege policy change is needed.
- `src/wayland/input.rs` maps stream coordinates to the selected output's logical
  rectangle and resolves keysyms against the compositor's XKB keymap. It adjusts
  Shift for text symbols, and uses `zwp_virtual_keyboard_v1` for text absent from
  the physical keymap. Normal keys and shortcuts pass through Aizome's ordinary
  input handling. Disconnect releases held keys and buttons.
- `src/wayland/clipboard.rs` uses `zwlr_data_control_manager_v1` for text/PNG in
  both directions. Clipboard ownership is synchronized before paste input. Pipe
  transfers have a size limit and deadline and run away from Wayland dispatch.
- Linux PulseAudio/PipeWire-Pulse audio, microphone, file transfer and recording
  retain their existing implementations. Native cursor shape capture, window
  selection, DMA-BUF/hardware encoding, and viewer-driven output configuration
  are separate work.

The web protocol's hello includes `captureBackend` (`x11` or `wayland`) for
recording comparison provenance. The browser decoder and presentation are
unchanged. `--show-cursor` determines whether cursor pixels enter the video;
keep that setting equal when comparing clients.

## Native integration check

`bench/aizome-smoke.py` starts a private headless Aizome with two outputs, one at
scale 2, its own runtime/config/state directories, and `--no-xwayland`,
`--no-shell`, `--no-session`. It never connects to the login compositor. The
native `wayland-probe` surface supplies known pixels and reports delivered
keyboard, pointer and scroll events. A real Voxl process streams to a WebSocket
client decoded with FFmpeg/PyAV. Clipboard checks use wl-copy/wl-paste against
that same isolated socket.

```sh
cargo build --locked --examples
cargo build --locked
# Python needs websockets, av and pillow; wl-clipboard must be on PATH.
python bench/aizome-smoke.py
# Also exercise the actual browser viewer (requires Python playwright):
VOXL_BROWSER=/path/to/google-chrome python bench/aizome-smoke.py
```

`AIZOME_BIN` selects the compositor executable and `VOXL_BIN` selects the Voxl
build. `AIZOME_TEST_RENDERER=gles` exercises Aizome's GLES path (which may be
software Mesa on a headless machine). The script retains logs, result JSON, and
original-resolution PNGs in the printed temporary directory. Those images are
test-fixture evidence, not screenshots of the user's desktop.

These are component/integration checks. They do not qualify a Sekka ISO, physical
hardware, or establish performance numbers. Compare release builds on the same
compositor, renderer, output size/scale, workload, codec settings and link. Measure
content-to-decode separately from input-to-visible-display latency; the latter
includes the browser/native presentation path we eventually want to compare.

### Initial validation (2026-11-10)

- Built the Nix release package with Sekka's locked Nixpkgs, including all three
  TypeScript projects and 27 passing Rust unit tests (one upstream encoder
  microbenchmark is ignored). Rust formatting and Clippy pass with the upstream
  exceptions above. `npm run check` passes formatting, lint and TypeScript checks.
- Ran the native suite with Pixman and GLES, including a real headless Chrome
  viewer. The isolated compositor was Aizome 0.1.0 with logic build
  `0.1.0+6b49791716909c4f`, with no Xwayland or desktop shell. GLES used an NVIDIA
  RTX 5090 render node; this is headless rendering evidence, not validation of an
  installed desktop or physical display.
- Decoded native surface pixels, changed them through keyboard input, and checked
  pointer positioning on an offset scale-2 output, buttons, scrolling, Shift,
  Caps Lock, Unicode/IME, text and PNG clipboard exchange, idle capture, `/wait`,
  resize/rotation, and read-only command refusal. Disconnect and Stop Sharing
  release held keys/buttons; revocation exits without starting another share.
- Repeated the suite with the Nix release binary and Chrome on GLES. Separately
  streamed and decoded a disposable Xvfb display, checking that `--display`
  overrides inherited `DISPLAY` and selects X11 even with `WAYLAND_DISPLAY` set.

Audio/microphone, file transfer, recording, and macOS/Windows behavior have not
been exercised by this native integration suite. No latency comparison is claimed.
