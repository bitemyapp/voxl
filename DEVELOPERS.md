# Developing the Aizome Wayland backend

This repository is a fork of tembo/voxl, based initially on
`b00f3b8b8c156b14e0c75abdfcf5cae9a14a865f`. Keep the server's H.264, WebSocket,
WebCodecs, authentication, recording and flow-control paths shared across capture
backends. The separate `native/` package implements the native Remote Desks viewer;
`media/` provides a small FFmpeg/CUDA ownership boundary. CPU and browser paths
remain comparison baselines and fallbacks. Measurements in the work log are
preliminary component results, not claims about physical-display latency.

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
  used for output geometry, input, and optional retained-paint subscriptions. It uses `--aizome-socket`, `AIZOME_SOCKET`,
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
  selection and viewer-driven output configuration
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

## Native viewer and optional GPU build

```sh
nix-shell --arg withRust true --arg withGpu true
cargo build --release --features gpu --locked
cargo test --features gpu --locked
nix-shell native/shell.nix
cargo build --release --manifest-path native/Cargo.toml --locked
cargo test --manifest-path native/Cargo.toml --locked
```

Use the same root-flake Nixpkgs override described above. `default.nix` accepts
`withGpu = true`; `native/default.nix` builds the viewer and deterministic scene.
The source filters include only these packages' own sources and the shared media
shim. NVIDIA driver libraries are found through Nix's `addDriverRunpath` after
RPATH shrinking. The GPU server does not depend on FFmpeg; the native viewer does.

`--video-backend=cpu|auto|gpu` selects capture/encoding (default CPU). Auto falls
back on initialization/capability failure; revocation is terminal and never creates
a replacement share. A runtime GPU/capture failure currently terminates the server;
restart with `cpu` if the hardware has become unavailable. Strict GPU mode reports
unsupported hardware. GPU capture currently requires Aizome's GLES renderer,
DMA-BUF output capture and NVIDIA NVENC API 12.1 or newer. AMD/Intel use the CPU path.

`--gpu-preset=P1..P7` selects NVENC's preset (default P1).
`--gpu-quality=1..51` sets its VBR target quality (default 24; lower is better).
This is not x264 CRF: match image quality and bitrate when comparing encoders.
`--paintover-crf` supplies the idle NVENC target quality too; paintover delay/burst
and adaptive bitrate/flow control remain shared. H.264 has no B frames or lookahead.

The server allocates two compositor-compatible GBM DMA-BUFs. A capture becomes
readable only after the compositor's `ready` fence. Imported XRGB GL textures are
not NVENC-registerable on the tested driver, so a GPU blit into persistent RGBA8
textures precedes NVENC. Encoding finishes before that capture buffer is reused.
Only compressed video crosses to the CPU; explicit `/screenshot` requests read back
pixels. The GPU worker releases driver resources before process shutdown.

The viewer's CPU decoder uploads I420/NV12 planes to persistent GL textures. CUDA
decode uses CUDA/GL interop copies, keeping source AVFrames alive until their copy
event completes; it does not download decoded frames. Textures unregister before
decoder teardown or geometry changes. Auto decode can reset to CPU and request a
fresh keyframe after a device/presentation error. Decoder queues, incomplete-frame
sets and packet dimensions are bounded; stripe groups present atomically.
Auto decode is now the default; `--decoder=cpu` remains the explicit baseline.

Native playback decodes the existing stereo 48 kHz Opus stream through libopus
into SDL. F8 toggles audio and `--no-audio` starts muted. The playback queue is
capped at 80 ms; stale queued sound is discarded after a network burst, and
disconnect/mute resets codec and playback state. Audio-device failure leaves
video usable. `bench/native-faults.py` uses a private compositor and SDL dummy
audio to test real CPU fallback with CUDA devices hidden, strict GPU refusal,
authentication errors, invalid geometry/JSON/packets, duplicate stripes,
incomplete-frame bounds, and an eight-second audio burst without an eight-second
playback backlog. This is not physical audio-device validation.

`bench/native-smoke.py` verifies actual pixels on a second isolated Aizome, then
injects input through that compositor and verifies it returned to the source.
`VOXL_EXTENDED=1` adds held printable keys, focus-loss release, resize/rotation and
server restart. Select `VOXL_VIDEO=cpu|gpu`, `VOXL_DECODE=cpu|auto|cuda`,
`VOXL_STRIPES=N`, `VOXL_BIN` and `VOXL_VIEWER` explicitly.

`bench/native-perf.py` uses the same receiving-compositor marker observer for
native and Chrome Wayland viewers. It retains raw frame ages, original screenshots,
server counters, process-tree CPU/RSS, binary hashes and compositor provenance.
Use `--viewer=local|browser|cpu|cuda --encoder=cpu|gpu --size=1920x1080
--workload=scroll|animation|drag|idle`. No fixture connects to the login compositor.
The local control uses the same observer on the source; capture requests can affect
compositor scheduling, so do not subtract that control as a pure observer overhead.
No physical scanout, network-clock synchronization or ISO qualification is implied.
See [the measurement report](docs/NATIVE-PERFORMANCE.md) for controlled comparisons
and their limits, and [the ongoing work log](docs/REMOTE-DESKS-WORK.md) for evidence
and remaining work.

## Optional Aizome retained drawing

The companion Sekka branch supplies `aizome_remote_paint_v1`, a data-only producer
protocol, and the separate Sukumo package `aizome-remote-renderer`. Build them
through Sekka's root flake and its existing development shell:

```sh
# In the Sekka checkout:
nix build .#aizome-remote-renderer
nix develop .#aizome -c cargo build --release \
  --manifest-path desktop/aizome/Cargo.toml -p aizome-comp -p aizome-comp-logic
nix develop .#aizome -c cargo build --release \
  --manifest-path desktop/aizome/sukumo/Cargo.toml -p sidestep-ui --example remote_scene
# In the Voxl checkout, with the matching compositor running:
AIZOME_REMOTE_RENDERER=/path/to/renderer/lib/libaizome_remote_renderer.so \
  voxl-viewer http://localhost:6901/ --native-widgets
```

`--renderer=PATH` also selects this **local** library; no path, executable code,
resource URL, or event handler comes from the peer. `native/default.nix` accepts
`aizomeRenderer` and wraps the viewer with its library path when supplied. The
renderer builds from Sukumo's own workspace, with a narrow first-party source
filter; it does not link the compositor or privileged helpers.

Sukumo records paint batches with their individual damage clips and ordering.
Fonts and decoded premultiplied images are cached assets. The compositor asks for
a full repaint with a fresh dictionary when a scene becomes eligible or a viewer
joins. A sealed memfd accompanies the ordinary surface commit; Smithay applies it
with the same synchronized-subsurface/fence rules as the buffer. Ordinary buffers
remain authoritative and continue to be painted for local display and fallback.

An IPC connection first subscribes, then requests `remote-scene` with `capture`
(the ext-image-copy-capture Wayland object ID), `output`, and `enabled`. Aizome
compares Unix peer credentials with the owner of that live capture resource. It
only exports the first visible, opaque surface covering the output, at native
scale without a transform or crop. The share must exclude the host cursor.
Other toolkits, extra surfaces in front, tiles, Core Animation trees, image-shaped
clips, and incompatible state use pixels. A revoked capture immediately loses its
retained subscription. Window selection and mixed pixel/native scene graphs are
not implemented; this is drawing replay, not a serialized widget tree.

The websocket commands are `scene,enabled,nonce,codec` and `sa,nonce,sequence`.
Native viewers request `zlib` by default; `--scene-compression=none` keeps the raw
transport for comparisons. Binary `0x20` messages carry a little-endian nonce and
transport sequence (four bytes each), followed by the versioned paint frame.
Binary `0x21` adds the expanded byte length (four bytes), then a sync-flushed zlib
message. A small dictionary persists across messages in one viewer epoch, so
repeated drawing attributes need not cross the network again. Declared and actual
expansion, message boundaries and transport sequences are bounded and checked;
errors discard that epoch and return to video. New connections/toggles reset the
dictionary. The shared framing implementation is `src/retained.rs`. The sequence inside that paint
frame is independent and guards the retained dictionary. A new viewer epoch
invalidates queued messages from an earlier toggle, geometry, or connection.
After a valid full native image the viewer sends `v,0`; pixel fallback clears the
old image and requests a complete video keyframe. A simultaneous browser viewer
continues to receive normal video. Video encoding can sleep when all viewers use
native replay.

The limits are 16 MiB per paint frame, 16 million pixels, 8,192 operations,
64 paint batches, bounded group nesting, and 64 MiB of cached assets. Remote font
registrations share a content-deduplicated process budget rather than growing on
each reconnect. The transport allows eight unacknowledged frames and 32 MiB of
retained websocket data across dictionary/worker restarts; dequeueing a message
does not release its budget before the socket write completes. ACKs cannot free
unwritten messages. One IPC worker per client coalesces rapid toggle changes;
a stalled replay returns to video.

`bench/native-scene.py` runs the real Sukumo fixture between two private Aizomes.
Set `AIZOME_BIN`, `AIZOME_SCENE_FIXTURE`, and `AIZOME_REMOTE_RENDERER` to matching
builds; `VOXL_SCENE_SCALE=1.25` checks fractional scaling, and `VOXL_VIDEO=cpu`
keeps the software server baseline. It checks exact whole-window and damaged
pixels, input, F9, independent mixed clients, popup fallback/recovery, stalled
clients, rapid toggles, capture ownership and revocation. `bench/native-faults.py`
adds malformed/stale/gapped retained states when the renderer variable is set.
`bench/native-perf.py --source=sukumo --native-widgets --viewer=cuda --encoder=gpu`
uses the same independent marker observer as the video/browser measurements.
Omit `--native-widgets` for the same application over ordinary video.
