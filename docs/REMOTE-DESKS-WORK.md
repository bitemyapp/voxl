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

## Constrained-link evaluation in progress

Retained checkpoint `9eed4ed` and Sekka `ccf4dc4a` are pushed to draft PRs #2/#382.
Portable producer warnings were fixed in Voxl `4e17ce9`; all five CI jobs pass,
including Windows smoke. Final package outputs at that checkpoint include
server `/nix/store/9ayyvbczs26nwc336idbym1i9y0bacn4-voxl-0.3.4` and renderer
`/nix/store/z1jbq45fv5ahklkrgr7ndfl25wxjyki3-aizome-remote-renderer-0.1.0`.

Added a private bounded TCP link emulator, calibrated against a 1 Mbit/s echo
with 25 ms propagation per direction: measured 1.09976 s versus expected
1.098576 s. It models reliable ordered bytes, propagation and serialization,
not real packet loss/congestion. No host networking is altered. Python 3.12+
server shutdown waits for active transports, so teardown must cancel/close those
before waiting for the listener; open-client teardown is explicitly checked.

A 5 Mbit/s / 40 ms round-trip experiment exposed retained startup backpressure:
the initial asset transfer could fill eight outstanding messages and incorrectly
trigger pixel fallback. The worker now stops reading IPC until delivery ACKs free
space; a stalled viewer still times out and sustained backpressure after bootstrap
returns to fresh video instead of accumulating increasingly old paint. Socket
writes and ACKs signal the same progress condition to avoid missed wakeups.
The first fixed 4K Sukumo run stays native, 23/25 ms median/p95, 59.26 valid
marker frames/s, zero video bytes and 472,051 retained bytes per 15 seconds:
`/tmp/voxl-perf-7jlwvo3r`. Live replay regression passed again at
`/tmp/voxl-native-1ps0klo1`. Later pressure-age/wakeup refinements still need the
same integration regression before committing.

High-motion 4K video at this link budget is more difficult. GPU P1 queued large
encoded frames (about 242 ms median / 337 ms p95); simply changing VBR to CBR,
raising presets P3/P5/P7 or explicitly permitting QP 51 did not fix that. These
experimental encoder changes were removed. Evidence: `target/perf-baseline/
link-{5mbps-40ms,rate-control,presets}/`, `/tmp/voxl-perf-26yvl91c` (NVENC trace),
and `/tmp/voxl-perf-6i6mwsu7` (explicit QP bound). The trace requested 2 Mbit/s
but still produced ~30 KB frames at reported average QP 47. A GPU-specific
byte window is now being tested to prevent oversized frames from building a long
queue while retaining the newest captured content.

At the same budget CPU H.264 frequently corrupts the timestamp/checksum marker;
its resulting apparent latency/fps cannot be compared fairly. Enlarging cells
from 8 to 16 pixels did not repair that. The harness now rejects latency/fps
scores whenever the marker integrity check fails, retains diagnostic observations,
and records absolute final server state alongside deltas. Earlier reported LAN
and Sukumo cells have no invalid markers. Broader fixed-frame quality and the
new GPU byte-window measurements are the next checks.

## Link and glyph-cache validation

The controlled GPU byte-window matrix is complete: two repeats per old/new
server and LAN/5 Mbit/s 40 ms link. Saturated 4K animation changes from
242–243 ms median / 336–339 ms p95 to 130–133 / 140 ms, trading roughly 15.8 fps
for 7 fps. LAN remains 8/9 ms at about 60 fps. The report preserves this tradeoff,
not a universal network-performance claim. Six additional fixed-frame quality
cases are complete; GPU improves overall SSIM on these samples but has worse
4K-animation edge error than CPU. Reports are in `docs/benchmarks/`.

A 1 Mbit/s startup probe found that full-font dictionaries exceeded the two-second
ACK guard. Sekka `4e1e2eee` now exports final-scale glyph masks from the actual
shared text cache, with face/variation/size/subpixel identity, cached placements
and exact Alpha8 blending. Full-font transfer remains selectable and handles
color glyphs. Byte/expanded-operation budgets remain enforced. Four replay tests
cover six scales, synthetic/variable faces, groups/shadows, recoloring, missing
and duplicate assets; five existing glyph raster tests pass (two performance
benchmarks remain ignored). Protocol/ABI tests and all-target Clippy pass.

Packaged renderer `/nix/store/8lrjhy21djz5gbnf44359icxjyq34pc6-aizome-remote-renderer-0.1.0`
passes the CPU/full-font baseline integration at `/tmp/voxl-native-69_cxry2`.
Cached-glyph GPU integration at 1.25x passes all 12 checks with zero differing RGB
pixels at `/tmp/voxl-native-7_sxozn1`. The new pressure guard allows a bounded
two-second catch-up after first ACK, because the initial video keyframe can leave
a temporary backlog even when steady replay fits. Truly stalled frames still
expire after two seconds; sustained slow acknowledgement still selects video.

Ten serial glyph/link samples, two per configuration, are preserved in
`target/perf-baseline/glyph-link/` and the committed report. At 5 Mbit/s / 40 ms,
readiness is 1,517–1,518 ms with fonts versus 304 ms with masks; bytes sent by
readiness fall from ~762 KB to ~14 KB. At 1 Mbit/s / 100 ms, cached replay starts
in 510–556 ms and remains native at ~59.24 fps, 56 ms median / 59–60 ms p95.
LAN remains 2/3 ms. Readiness is a coarse 50 ms-polled process/window/network/
renderer observation; it is not first scanout. Same-scene slow-link video controls
and final package/regression checks are underway.

Final component checks: 33 GPU-feature and 32 CPU-only server tests pass (one
existing benchmark ignored in each), both Clippy configurations pass with the
three documented upstream/new-toolchain exceptions, and all seven native tests
plus strict Clippy pass. Web format/lint/types pass in the pinned Node environment.
The expanded viewer fault suite passes 28 cases at
`/tmp/voxl-native-faults-y85jt00s`, including malformed/missing/duplicate glyph
assets, expanded-operation bounds and successful video fallback with the older
renderer library.

Added source-owner authorization: `--native-widgets=true` is required on the
server, independently of F9 in the viewer. Cached assets/drawing can include
clipped or occluded data beyond the final visible pixels, so ordinary pixel
sharing must not implicitly grant that capability. A unit regression checks
refusal before worker/IPC activation; the live suite now also opens a second,
default read-only server and verifies that it refuses state while delivering
ordinary video. All 13 packaged replay checks pass with exact 1.25x RGB output
at `/tmp/voxl-native-doj4ycy7`.

Current rooted Nix outputs: server
`/nix/store/976hcg2ib2d60f818qsi5d7hk1lww3cj-voxl-0.3.4`, viewer
`/nix/store/v5g1fw5mbwl19adiq1v6wn3jwlg7qasx-voxl-viewer-0.1.0`, and its default
renderer `/nix/store/aw568pg3q39rrq3341wiza7mpvxr5pac-aizome-remote-renderer-0.1.0`.
The renderer comes from the local Sekka root flake. Remaining closure checks use
only the graphics-driver library directory, without the development library path;
final timing confirmation will use these exact packaged binaries.

## Final package confirmation

Source-owner checkpoint `4fc7eeb` passes all five GitHub CI jobs, including GPU/
native builds, Linux package verification, macOS and Windows smoke. Sekka head
`686e42c5` contains glyph caching plus source-authorization documentation; its
trust check passes and repository policy skips the build jobs.

The locally built compositor needs its development EGL search path. The fixture
now supports `VOXL_PACKAGE_LIBRARY_PATH` to isolate just the packaged server and
viewer while leaving that fixture dependency explicit. With only
`/run/opengl-driver/lib` for those two packages, all 13 retained checks pass at
`/tmp/voxl-native-16h59hpn`, and extended GPU/CUDA video checks pass at
`/tmp/voxl-native-diqgxc23`, including held keys, focus-loss release, resizing,
rotation, server restart/reconnect and capture revocation.

Four serial final-package timing cells are saved in `target/perf-baseline/
final-packaged/` and `docs/benchmarks/2026-11-10-final-packaged.json`. Same 4K
animation: CPU/browser 63/71 ms median/p95, GPU/native 8/9 ms; server/viewer CPU
208.09/98.69% versus 2.08/3.49%. Packaged retained replay remains 2/3 ms on LAN
and 56/61 ms on the 1 Mbit/s/100 ms link, with zero video bytes after warmup.
All markers are valid. Exact package paths and hashes are recorded. No release,
ISO or physical-display qualification is implied.
