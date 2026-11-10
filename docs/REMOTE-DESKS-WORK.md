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
