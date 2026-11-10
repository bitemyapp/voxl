# Native Remote Desks work log

Authorized 2026-11-10: implement a native viewer, GPU capture/encoding/decoding/presentation, preserve CPU baselines/fallbacks, measure and optimize, then explore optional user-toggleable Aizome-native state reification with pixel fallback. No publication. All integration tests use isolated compositors, never the login desktop.

Starting point: `0ac51d281a2deb2d0c92e89577ff4dafacb06efb` (native Wayland backend, PR #1, CI green). Work branch: `codex/native-remote-desks`; keep prior PR intact until review/merge.

## Sequence

1. Establish native viewer decoding/presentation and input, a reproducible frame-marker/latency benchmark and the CPU/browser baseline.
2. Add optional GPU media support, DMA-BUF capture and hardware encoding, hardware decode/presentation, explicit selection and automatic fallback. Reuse server authentication/flow control and keep queues bounded.
3. Compare release builds on identical workloads (idle, text/scroll, animation/video, window drag, resize, constrained link), including CPU/GPU cost, quality and visible latency. Optimize measured bottlenecks.
4. Add versioned optional Aizome state updates. Explore compositor surface/window state and Sukumo display operations; unsupported or inconsistent content stays pixel based. Native viewer exposes a live toggle.
5. Exercise reconnect/revocation, resize/rotation/scales, slow clients, malformed input, fallback and teardown; document supported hardware, results and limits; publish reviewable source branches/PRs only.

## Environment

- Host GPU: NVIDIA RTX 5090, driver 615.78.08; render node /dev/dri/renderD128.
- Locked Nixpkgs: use Sekka's root flake (`builtins.getFlake "/home/callen/work/sekka"`).
- Aizome already advertises DMA-BUF capture formats/modifiers and renders directly into client targets; no compositor modification is needed for that path.
- Sekka checkout has unrelated user edits; preserve them and use a separate managed worktree if compositor/toolkit changes are needed.
- Baseline executable is retained at `result/bin/voxl`; baseline native validation is in `target/aizome-validation/`.

## Progress

- Initial native Rust/SDL2/OpenGL viewer in `native/`, separate Cargo package and lockfile; shared libavcodec ownership shim in `media/`.
- CPU integration passed between two isolated GLES Aizome instances: `/tmp/voxl-native-m2k8wz54`.
- NVDEC -> CUDA/OpenGL texture transfer passed the same test with no decoded-frame CPU download: `/tmp/voxl-native-fs8d3b2i`. First few frames include device/decoder warmup; these are correctness tests, not performance results.
- Rooted development dependencies under `target/native-deps` and Python suite under `target/native-python` because unrooted temporary Nix environments were garbage-collected.
- Viewer now validates dimensions/packet headers, atomically presents multi-stripe frames, reconnects with stale-input discard, and uses event-driven SDL wakeups. Auto decode can recover to CPU after CUDA/GL failure. Input/clipboard/audio parity, UI controls, broader fault tests and profiling remain in progress. Default CPU until comparisons justify auto policy.

## GPU capture/encode design being implemented

Use Aizome's advertised capture render-node/formats/modifiers. Allocate two reusable GBM DMA-BUF targets and import into a surfaceless EGL/OpenGL context. The RTX 5090 driver rejects direct NVENC registration of imported XRGB storage (NVENC 23), so the working path blits into persistent RGBA8 GL textures registered with NVENC. This is an extra GPU copy, not end-to-end zero-copy. GPU capture alternates buffers so the compositor never writes the texture currently being encoded. ext-image-copy-capture `ready` follows Aizome's GPU fence. NVENC bitstream completion permits reuse; no full-frame CPU readback or CPU color conversion. Snapshot/readback happens only for explicit screenshot requests. CUDA decode/presentation is separate from capture hardware, so CPU encode + GPU viewer and GPU encode + browser remain valid comparison cells.

- Full GPU server + native CUDA viewer visual/input smoke passed: `/tmp/voxl-native-h4zqnjwm`.
- GPU server full Wayland + Chrome integration passed, including scale, text/PNG clipboard, idle, resize/rotation, read-only refusal, disconnect and share revocation: `/tmp/voxl-aizome-ki3xuw9o`.
- Found/fixed driver shutdown hang: stop and join the GPU worker, releasing GL/NVENC resources before process exit. Initialization acceptance handshake prevents a timed-out GPU initialization from racing a CPU fallback.
- GPU-feature root unit tests: 27 passed, one upstream benchmark ignored; Clippy passed with documented upstream lint exceptions. Native protocol boundary tests pass; native Clippy passes.
- Four-stripe CPU native smoke passed (`/tmp/voxl-native-2vt7pj1t`). Expanded CPU and CUDA suites passed held printable keys, modifier release on focus loss, resize/rotation and reconnect (`/tmp/voxl-native-gfvg6pcg`, `/tmp/voxl-native-h1twl4ty`).
- Both Nix packages built. Package-only runtime smoke exposed Nix RPATH shrinking removing the dynamically loaded NVIDIA encoder search path; adding `addDriverRunpath` in postFixup fixed it. Installed-package GPU/native extended suite passed (`/tmp/voxl-native-j9r446ic`).
- GPU server auto fallback on Pixman passed the full Wayland suite (`/tmp/voxl-aizome-h9gg1bw9`). Revocation during initialization is a typed terminal failure, distinct from capability fallback. Initial acceptance handshake now waits for geometry publication.
- CUDA copies retain the source AVFrame until a CUDA event confirms completion before allowing decoder storage reuse. Viewer conversion honors BT.601/709/2020 and full/limited range. Native key handling preserves printable key lifetimes, modifier-changing releases and multi-character IME commits; text paste synchronizes the clipboard before Ctrl+V.
- GPU pacing now matches the software path's 15% source-jitter allowance. Added independent NVENC preset/target-quality controls and bounded idle refinement; CPU quality flags retain their meaning. GPU encode is still NVIDIA-only, runtime GPU/capture failure terminates safely, and broader fallback/fault testing remains in progress.

## Preliminary measurements (not final results)

`bench/native-perf.py` uses a native deterministic GL scene and a separate persistent Wayland observer of the marker rectangle in the receiving compositor. Both viewers use identical observation. Full-resolution PNGs and raw frame ages are retained. This measures headless compositor-visible age, not physical scanout. Local-source control: 60 fps, p50 2 ms, p95 3 ms (`/tmp/voxl-perf-ex2ayy4w`).

First 10-second release-build run, 1920x1080 scroll, 60 fps, 30 Mbit/s cap, nominal quality 24 (x264 CRF and NVENC quality are not equivalent):

| Encoder | Viewer         | Median / p95 age | Server / viewer CPU (one core = 100%) | Evidence                               |
| ------- | -------------- | ---------------- | ------------------------------------- | -------------------------------------- |
| CPU     | CPU native     | 14 / 21 ms       | 20.0 / 5.1%                           | `/tmp/voxl-perf-e6yphqxa`              |
| GPU     | CPU native     | 16 / 25 ms       | 2.1 / 7.2%                            | `/tmp/voxl-perf-58iiniuj`              |
| GPU     | CUDA native    | 12 / 20 ms       | 2.1 / 3.3%                            | see `/tmp/voxl-perf-gpu-cuda-1.log`    |
| CPU     | Chrome Wayland | 38 / 59 ms       | 19.8 / 17.3%                          | see `/tmp/voxl-perf-cpu-browser-1.log` |
| GPU     | Chrome Wayland | 24 / 54 ms       | 2.1 / 19.4%                           | see `/tmp/voxl-perf-gpu-browser-1.log` |

All observed markers passed checksum. Browser viewport verified 1920x1080, DPR 1. Needs repeated runs, codec quality comparison, representative workloads/resolutions and explicit browser hardware-decode provenance. GPU pacing (~59.2 fps) is a measured optimization target. Subsequent event-loop/network changes have not yet been measured.

The observer requests the receiving compositor's current scene through screencopy;
it can re-render that scene before the next output redraw. These figures therefore
measure capture-visible scene age, not an already-scanned-out framebuffer. Both
viewers use the same boundary. In particular, the local control must not be
subtracted as a pure measurement-overhead correction.

Fixed-frame quality comparison now runs separately (`--quality`), freezing the
same deterministic source frame (239 at 1080p/60 Hz) in both runs and retaining
the lossless reference, encoded moving frame, idle-refined frame, raw H.264 and
FFmpeg SSIM logs. First scroll sample at nominal 24: CPU moving SSIM 0.997701,
RGB PSNR 44.42 dB; GPU moving SSIM 0.999949, PSNR 48.05 dB. GPU background has a
roughly one-level RGB conversion offset; its edge error is lower in this sample.
This is a single synthetic workload, not a general quality equivalence claim.
Evidence is indexed by `/tmp/voxl-quality-{cpu,gpu}-fixed.log`.

Package verification caught Cargo excluding encoder sources nested under the
separate `media` package. Server-only GPU allocation/encoding now lives in
`src/gpu/`; `media/` owns viewer decoding/interoperability. GPU-feature
`cargo package --locked` verification passes with this layout.

## Controlled repeats and capture scheduling

Checkpoint `54ca815` is in draft PR #2; CPU-only Clippy warning repaired in
`42d7349`, all five CI jobs green (including Windows smoke). Three serial repeats
per cell at 1080p and 4K are recorded in
`docs/benchmarks/2026-11-10-scroll-baseline.json`, with raw evidence preserved
under `target/perf-baseline/scroll-matrix/evidence/`. Chrome 157.0.8103.38 uses
NVIDIA ANGLE/OpenGL compositing; its reported hardware video profile list is
empty. Actual WebCodecs decoder selection is still being investigated.

At 1080p the three median scene ages were CPU/browser 48/42/51 ms,
CPU/native 13/13/13 ms, GPU/native 12/12/12 ms, GPU/browser 53/50/47 ms.
At 4K: CPU/browser 38/55/64 ms, CPU/native 24/24/24 ms, GPU/native 16/15/16 ms,
GPU/browser 40/56/57 ms. Median server/viewer one-core CPU percentages at 4K:
CPU/browser 110.88/75.27%, GPU/native 2.41/3.58%.

Aizome's output capture used display redraws to gate possible damage, although
each capture renders the current scene independently. A separate capture scene
revision removes that wait while preserving actual damage tracking and fences.
Sekka commit `ca94cbe8`, draft PR #382, contains the change and headless refresh
configuration support. Both new 1 Hz regression tests pass and fail against the
former gating; 26 component tests pass, with linear GPU readback explicitly
skipped where unsupported. Release Clippy passed. Protected comparator builds
live in the managed Sekka worktree's `desktop/aizome/target/capture-{optimized,vblank}`.
Two serial repeats with otherwise identical builds and streaming binaries:
1080p GPU/native median 12 -> 4 ms, p95 19–20 -> 5 ms; 4K median 15–16 -> 7 ms,
p95 23 -> 8 ms. Index: `target/perf-baseline/capture-comparison/index.json`.
This remains capture-visible scene age, not physical scanout latency.

An additional direct-import experiment with ARGB8888 also failed NVENC texture
registration (23). Registering fresh fallback textures after that failed probe
also failed on this EGL driver. The experiment was removed; the established GPU
blit path remains. Evidence: `/tmp/voxl-native-0j_thjzv`. Restored GPU path plus
optimized compositor passed extended native integration: `/tmp/voxl-native-r1u6leos`.

Native audio playback and default-auto decoding are now implemented. The real
viewer passed 11 fault/fallback/audio cases at `/tmp/voxl-native-faults-p7xn05wn`.
An eight-second audio burst decoded 400 packets while discarding 99 stale queues,
keeping playback bounded. No host audio device was used.

## Next: optional retained Aizome drawing

Implement a conservative first negotiation boundary: a supported fullscreen
Sukumo root surface can send retained drawing/damage updates to the native viewer.
Other toolkits, extra visible surfaces/popups, unsupported layer operations, or
inconsistent state retain/re-enter the existing pixel stream. The native viewer
will expose a live toggle. Once replay is ready it can stop its own pixel-video
subscription, avoiding encode/decode work when all viewers use native replay;
browser and mixed-viewer sessions retain normal video. Keep scene authorization
tied to the server's existing ext-image-copy-capture session, including revocation.
Use the actual Sukumo rasterizer through an optional bounded native renderer ABI
so text/path semantics stay identical, rather than guessing widgets from pixels.
This section records the implementation direction, not completed functionality.

## Retained drawing implementation and first integration evidence

The optional path is implemented across both topic branches (currently being
hardened, not yet a finalized supported release): Sukumo exports data-only paint
batches and cached fonts/images using a new double-buffered Wayland protocol.
Aizome applies them with Smithay's committed surface state and exposes them only
to a Unix peer whose PID owns the existing live ext-image-copy-capture output
session. The first visible surface must cover the output opaquely at native
scale; popups, tiles, overlays, transformed/cropped surfaces and unsupported ops
select ordinary pixels. Revocation ends both streams. A full repaint must replace
the entire canvas before it can establish a new retained dictionary.

The viewer loads the optional local `libaizome_remote_renderer.so` C ABI, using
Sukumo's actual rasterizer. F9 toggles replay live, default off; `--native-widgets`
enables it initially, `--renderer=PATH` or `AIZOME_REMOTE_RENDERER` selects the
local library. Only after a valid full image does it send `v,0`; fallback requests
a fresh video keyframe and clears the old image. Partial paints upload only their
damage to a persistent RGBA GL texture. Audio and input stay shared.

The renderer now belongs to the **Sukumo workspace** at
`desktop/aizome/sukumo/crates/aizome-remote-renderer`, separate from the compositor.
Root-flake package `.#aizome-remote-renderer` builds from a narrow local source
filter. First package succeeded at
`/nix/store/lcq6wjcngj5v879flvgg84301aw8q5n7-aizome-remote-renderer-0.1.0`.
Voxl's native Nix package accepts optional `aizomeRenderer` to wrap the viewer
with that library. This has not changed the running login desktop.

`bench/native-scene.py` passed at `/tmp/voxl-native-awu8uwp2` (1x) and
`/tmp/voxl-native-qjz_zzd4` (1.25x), with **zero differing RGB pixels** for the
whole text/path window and its partial button damage. It also verified F9 off/on,
video continuing for a simultaneous ordinary subscriber, a real popup forcing
video and restoring native replay on close, 200 rapid toggles using one IPC
worker, stalled replay falling back, denial of an unrelated PID using the actual
capture object ID, and share revocation. A sole retained viewer animated while
the server's encoded-frame count stayed unchanged.

The transport limits messages/assets/state and now also reserves actual queued
WebSocket bytes across worker/dictionary restarts. ACKs cannot release unwritten
messages. One worker per client coalesces toggle changes. Protocol and ABI tests
pass; exact raster tests cover ordered overlapping batches, fractional scales,
groups, image assets and missing-asset rejection. Native Clippy and five native
tests pass; server tests are 28 passed, one ignored. More fault cases, package
checks, retained-path measurements and broader quality/network tests remain.

## Compressed replay and measured checkpoint

Completed the identical Sukumo source comparison at 1080p/4K across local,
CPU/native, GPU/native, CPU/browser, GPU/browser and retained paths. Retained
replay is 2/3 ms median/p95 at both sizes; GPU/native video is 3/5 and 7/8 ms;
browser cells are 35–42 ms median. The raw drawing stream was inefficient
(~390 KB/s), so the transport now offers a bounded streaming zlib dictionary.
The native default requests zlib; `--scene-compression=none` preserves raw frames.
Shared framing checks expanded sizes, sync boundaries, contiguous sequences and
epoch resets. Malformed data cancels the dictionary and restores video.

Twelve serial comparison runs (three repeats per codec/resolution) measured
2/3 ms median/p95 throughout. Compression uses ~0.37 MB versus 4.68 MB per
12 seconds, ~92% fewer bytes. CPU is ~1% server and ~2% viewer. Warmup excludes
the initial asset dictionary. CPU H.264 still uses fewer bytes on this very small
animated UI, so native replay does not universally win bandwidth.
Reports and methodology: `docs/NATIVE-PERFORMANCE.md` and
`docs/benchmarks/2026-11-10-{retained-uncompressed,retained-compression}.json`.
Capture scheduling and optimized animation reports are now checked into the same
directory, preserving hashes and settings. Raw logs/screenshots remain under
`target/perf-baseline/`.

Compressed live integration passed all eleven checks, exact pixels included:
`/tmp/voxl-native-vgf5rzdj`. The expanded fault fixture passed 22 cases at
`/tmp/voxl-native-faults-83rff21j`, including independently compressed Python zlib
streams, malformed lengths/truncation and a fresh reconnect epoch. Native tests
are now seven passing; server tests 30 passed, one ignored; CPU/GPU Clippy and
web checks pass. A compositor review also prevented repeated producer rebinding
from accumulating permanent surface commit hooks. Package verification and
broader quality/network work continue before the next checkpoint.

The wrapped native Nix package built and passed the eleven live retained checks
at 1.25x with the rebuilt compositor: `/tmp/voxl-native-7hg3tkqd`.
Package: `/nix/store/53hs2lqk4xvcpkyb8wnqr0zdhn1q7y0d-voxl-viewer-0.1.0`.
The renderer library was selected by the package wrapper, without an explicit
test override. Release compositor Clippy/build and final web checks pass.
