# Native Remote Desks measurements

The native viewer and GPU media path substantially reduce measured scene age and
CPU use on the test machine. Optional Sukumo drawing replay reduces work further
for eligible UI scenes. These are component measurements on private headless
Aizome instances, **not physical display/input latency or WAN measurements**.

## What is measured

The source draws a monotonic timestamp, frame number and checksum into its
content. An independent Wayland screencopy observer reads the receiving
compositor's scene for both Chrome and native viewers. The resulting age includes
source painting, capture, transport, decoding/replay and the viewer's surface
commit. The observer can see that commit before physical presentation; it does
not measure scanout. Do not subtract the local control as an observer correction.

Hardware: RTX 5090, NVIDIA 615.78.08, one Linux workstation, separate source and
receiver Aizome processes, 60 Hz content, release builds. Measurements run serially
without concurrent builds or integration suites. JSON reports preserve binary
hashes, compositor build provenance, settings, counters, CPU and evidence paths;
raw observations and original screenshots remain under `target/perf-baseline/`.
CPU percentages use one core = 100%; summed process RSS can count shared pages
more than once. Chrome 157 reports NVIDIA ANGLE compositing; its actual WebCodecs
decoder implementation has not been established.

## Video: separate the viewer and encoder gains

The deterministic moving scene uses identical content and the updated Aizome
capture scheduler in every cell. Each cell below is one 12-second sample, so use
it as workload evidence rather than a confidence interval.

| Encoding / viewer | 1080p median / p95 age | 4K median / p95 age | 4K server / viewer CPU |
| ----------------- | ---------------------- | ------------------- | ---------------------- |
| CPU / Chrome      | 48 / 56 ms             | 64 / 72 ms          | 212.76 / 96.46%        |
| CPU / native CPU  | 6 / 8 ms               | 25 / 27 ms          | 219.08 / 43.25%        |
| GPU / Chrome      | 50 / 58 ms             | 61 / 69 ms          | 3.49 / 120.96%         |
| GPU / native CUDA | 4 / 6 ms               | 8 / 9 ms            | 2.41 / 3.66%           |

[Animation report](benchmarks/2026-11-10-animation-optimized.json).
Earlier scroll comparisons provide three repeats per cell with the former
capture scheduler: GPU/native medians were 12 ms at 1080p and 15–16 ms at 4K,
versus CPU/Chrome 42–51 ms and 38–64 ms respectively.
[Repeated scroll report](benchmarks/2026-11-10-scroll-baseline.json).

An independently controlled comparison changed only Aizome's capture scheduling:
1080p GPU/native median 12 → 4 ms; 4K 15–16 → 7 ms, two repeats each. Capture now
observes committed scene changes without waiting for the output redraw timer;
damage tracking and GPU completion fences remain in place.
[Capture comparison](benchmarks/2026-11-10-capture-comparison.json).

Two additional 4K drag-scene repeats measure CPU/Chrome at 55 ms median /
62–63 ms p95, versus GPU/native 7 / 8 ms. GPU/native server/viewer CPU is
2.16% / 3.33%, compared with CPU/Chrome 87.1–87.9% / 72.8–75.3%.
Separate idle samples encode no video frames or bytes after warmup; viewer CPU
under the common observer load is 5.99% for Chrome and 0.25% for native. Static
timestamps eventually expire, so idle runs make no latency/frame-rate claim.
[Drag, idle and soak evidence](benchmarks/2026-11-10-drag-idle-soak.json).

The GPU path captures into DMA-BUFs, blits into persistent NVENC-compatible GL
textures and encodes H.264 without CPU frame readback/conversion. CUDA decoding
transfers decoded surfaces into persistent GL textures without downloading them.
It is **not end-to-end zero-copy**: direct NVENC registration of the imported
capture textures failed on this driver. CPU capture/encoding, CPU decoding and
the browser remain available and are exercised independently.

## Optional native Sukumo drawing

The same actual Sukumo application paints text, paths, a button and a 60 Hz
damaged marker/bar region. All cells use the same application, dimensions and
observer. F9 drawing replay is default off and uses the actual Sukumo rasterizer
through a local optional library. The source still paints ordinary buffers.

| Path                                | 1080p median / p95 age | 4K median / p95 age |
| ----------------------------------- | ---------------------- | ------------------- |
| Local source control                | 1 / 2 ms               | 1 / 2 ms            |
| CPU video / Chrome                  | 37 / 52 ms             | 42 / 48 ms          |
| GPU video / Chrome                  | 35 / 46 ms             | 42 / 47 ms          |
| CPU video / native CPU              | 3 / 5 ms               | 10 / 12 ms          |
| GPU video / native CUDA             | 3 / 5 ms               | 7 / 8 ms            |
| Retained drawing, raw or compressed | 2 / 3 ms               | 2 / 3 ms            |

[Same-application comparison](benchmarks/2026-11-10-retained-uncompressed.json).
Video cells have one sample each. A separate comparison repeats raw and zlib
replay three times each at both resolutions using identical binaries. All twelve
runs have 2 ms median / 3 ms p95, about 59.3 visible frames/s and no video encoding
after warmup. Compression reduces roughly 4.68 MB per 12 seconds to 0.37 MB,
about **92% fewer retained wire bytes**, without a measured median/p95 penalty.
Server CPU is 0.83–1.00% and viewer CPU 1.83–2.16% across those runs.
[Compression repeats](benchmarks/2026-11-10-retained-compression.json).

At steady state the compressed replay is about 31 KB/s, versus GPU video
43–63 KB/s for this small UI. CPU H.264 is smaller still (roughly 7 KB/s), with
lossy pixels and more CPU work. Native replay therefore does not universally win
bandwidth. Startup font/image dictionaries are outside the warmed-up traffic
figures. Raw replay remains selectable with `--scene-compression=none`.

Whole-window and partial-damage integration comparisons have zero differing RGB
pixels at 1x and 1.25x. Protocol tests additionally cover images, groups,
overlapping batches and fractional scales. Fullscreen opaque Sukumo root paint is
supported; other toolkits, popups/overlays, tiles and unsupported operations select
the pixel stream. This is retained drawing replay, not a serialized semantic
widget tree or a general mixed native/pixel desktop compositor.

### Glyph caching and startup

Outline text now exports coverage from the source's actual Sukumo glyph cache,
then reuses mask ids with positions and colors. Variation/synthetic faces and
subpixel phases remain distinct; color glyphs keep full-font replay. The source
can select `AIZOME_REMOTE_GLYPHS=font` for the original baseline, or the benchmark
can select `--glyph-transfer=font`.

Two controlled 4K repeats on a 5 Mbit/s / 40 ms link reduce native readiness from
1,517–1,518 ms with whole fonts to 304 ms with masks. Retained bytes sent by that
point fall from about 762 KB to 14 KB, roughly **98% less**. This readiness metric
includes process/window/network/renderer startup and uses 50 ms polling; it is
not physical presentation or an exact first-packet size. LAN readiness is about
251–252 ms for both paths, where this transfer is not the bottleneck.

At 1 Mbit/s / 100 ms, cached replay starts in 510–556 ms and sustains about
59.24 visible frames/s, 56 ms median and 59–60 ms p95 scene age, with no video
bytes during measurement. Whole-font startup previously exceeded the two-second
ACK guard on this link and fell back to video. Cached LAN replay remains
2 ms median / 3 ms p95; 5 Mbit/s / 40 ms replay is 23 / 24 ms. These are reliable
link emulations, not real WAN measurements.
[Glyph-cache and link repeats](benchmarks/2026-11-10-glyph-cache-links.json).

On that same 1 Mbit/s / 100 ms link, single-sample video controls have median /
p95 ages of CPU/Chrome 62/66 ms, CPU/native 62/68 ms, GPU/native 65/67 ms and
GPU/Chrome 67/69 ms. Propagation dominates here: native video has no clear
latency advantage over CPU/Chrome for this small damaged UI. GPU/native uses
about 2.08% server and 3.24% viewer CPU, versus CPU/Chrome 57.39% and 66.21%.
CPU video sends fewer bytes; lossless native drawing avoids the codec and has
the lowest measured age (56/59–60 ms).
[Slow-link same-scene video controls](benchmarks/2026-11-10-sukumo-slowlink-video.json).

A 90-second 4K replay run on the 1 Mbit/s / 100 ms link delivers 5,319 retained
updates without reverting to video: 56 ms median / 60 ms p95 / 63 ms p99.
After warmup, server RSS grows by zero and viewer RSS by 384 KiB. This is a
bounded-duration soak, not proof of indefinite leak-free operation.
[Soak report](benchmarks/2026-11-10-drag-idle-soak.json).

## Constrained reliable links

`bench/link.py` provides a private, bounded TCP proxy with byte serialization and
fixed propagation in both directions. It changes no host network settings. Its
64 KiB echo calibration at 1 Mbit/s and 25 ms each way measured 1.100 seconds
against 1.099 seconds expected. This is a reliable-link model, not packet loss
or real WAN TCP congestion behavior.

A complex 4K animation exposed hardware bitrate overshoot: at a 4.5 Mbit/s cap
on a 5 Mbit/s link with 40 ms round-trip propagation, large encoded frames queued
behind each other. The GPU path now limits outstanding encoded bytes using the
target bitrate, measured minimum RTT and one frame interval. It continues to
capture current content while waiting for delivery.

Two controlled repeats reduced median scene age from 242–243 ms to 130–133 ms,
and p95 from 336–339 ms to 140 ms. This trades frame rate for freshness under
overload: roughly 15.8 visible frames/s became 7 frames/s. Both LAN configurations
remain at 8 ms median / 9 ms p95 and about 60 frames/s.
[Controlled byte-window comparison](benchmarks/2026-11-10-gpu-byte-window.json).
CBR, higher NVENC presets and an explicit maximum-QP experiment did not improve
this workload and were not retained. Codec rate control alone is not a bound on
the size of the next frame.

Some low-bitrate CPU-video runs corrupted the embedded timestamp marker, even
with larger marker cells. Their latency/frame-rate scores are rejected; raw
observations remain available for diagnosis. The benchmark now marks any such
measurement invalid instead of reporting apparently fast timestamps.

## Validation and limits

`bench/native-scene.py` checks exact pixels, remote input, live F9 off/on, mixed
video/replay clients, popup fallback/recovery, stalled consumers, rapid toggles,
capture ownership, source-owner opt-in and share revocation. `bench/native-faults.py` exercises 28
video/audio/replay fault cases, including independent Python zlib streams and
malformed lengths, sequences, reconnect epochs, cached-glyph limits and pixel
fallback with an older renderer library. Native/server unit tests and
Clippy cover the CPU and GPU feature configurations. See
[DEVELOPERS.md](../DEVELOPERS.md) for reproducible commands and protocol limits.

Video codec quality settings are not interchangeable: x264 CRF 24 and NVENC CQ 24
do not establish equal quality or bandwidth. Separate fixed-frame quality runs
retain lossless references, moving/idle-refined encoded images and SSIM logs.
The first 1080p scroll comparison measured CPU moving SSIM 0.997701 and GPU
0.999949. Additional fixed-frame samples are below; these are synthetic workload
comparisons, not a claim of equal codec settings or universal quality superiority.

| Workload        | CPU moving / refined SSIM | GPU moving / refined SSIM |
| --------------- | ------------------------- | ------------------------- |
| 1080p animation | 0.859480 / 0.918905       | 0.945260 / 0.949479       |
| 4K scroll       | 0.993172 / 0.994861       | 0.998678 / 0.998679       |
| 4K animation    | 0.872456 / 0.890930       | 0.898873 / 0.904461       |

GPU 4K animation has higher overall SSIM but worse measured edge error than
CPU encoding (3.78 versus 1.97 mean absolute levels). The report retains PSNR,
edge/error metrics and original image/H.264 evidence for assessing that tradeoff.
[Expanded quality report](benchmarks/2026-11-10-video-quality.json).

Hardware media support currently targets NVIDIA. Automatic initialization and
viewer decode fallbacks preserve CPU operation; a running server capture/GPU
failure terminates safely rather than silently reacquiring a revoked share.
No AMD/Intel hardware path, physical-display latency, physical audio-device test,
ISO qualification or production release is claimed by these results.
