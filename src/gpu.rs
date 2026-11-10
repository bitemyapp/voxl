//! GPU pipeline; the existing software pipeline remains the reference/fallback.
use crate::hub::{wait_any, Hub, Inflight, Out};
use crate::video::{flow_ok, RateCtl, VideoConfig};
use crate::wayland::gpu_capture::Capturer;
use bytes::Bytes;
use std::sync::{atomic::Ordering, mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum InitError {
    Unavailable(String),
    Revoked,
}
impl From<String> for InitError {
    fn from(value: String) -> Self {
        Self::Unavailable(value)
    }
}
impl From<&str> for InitError {
    fn from(value: &str) -> Self {
        Self::Unavailable(value.into())
    }
}
impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(e) => f.write_str(e),
            Self::Revoked => f.write_str("Wayland sharing was revoked or the output was removed"),
        }
    }
}
pub fn spawn(cfg: VideoConfig, hub: Arc<Hub>) -> Result<(), InitError> {
    let options = crate::gpu_media::EncoderOptions {
        fps: cfg.fps,
        kbps: cfg.max_kbps.min(8000),
        quality: crate::wayland::settings().gpu_quality,
        preset: crate::wayland::settings().gpu_preset,
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let (go, start) = mpsc::sync_channel(0);
    let owner = hub.clone();
    let worker = std::thread::Builder::new()
        .name("gpu-capture-encode".into())
        .spawn(move || {
            let mut cap = match Capturer::new(options) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            // Initialization may outlive the caller's timeout. Never modify the
            // shared hub or start streaming unless the caller accepts this worker.
            if tx.send(Ok(())).is_err() || start.recv().is_err() {
                return;
            }
            configure(&mut cap, &hub);
            if tx.send(Ok(())).is_err() {
                return;
            }
            loop {
                match run(&mut cap, &cfg, &hub) {
                    Ok(()) => {
                        drop(cap);
                        if hub.stopping.load(Ordering::Acquire) {
                            return;
                        }
                        match Capturer::new(options) {
                            Ok(c) => {
                                cap = c;
                                configure(&mut cap, &hub);
                            }
                            Err(e) => {
                                terminate(&hub, &e.to_string());
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        drop(cap);
                        terminate(&hub, &e);
                        return;
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(15))
        .map_err(|e| e.to_string())??;
    *owner.video_worker.lock() = Some(worker);
    go.send(()).map_err(|e| e.to_string())?;
    // Do not expose a server with zero geometry while the worker configures it.
    rx.recv()
        .map_err(|e| InitError::Unavailable(e.to_string()))?
}
fn configure(cap: &mut Capturer, hub: &Hub) {
    cap.register_remote();
    crate::wayland::input::set_output(cap.output_name.clone(), cap.width, cap.height);
    hub.width.store(cap.width, Ordering::Relaxed);
    hub.height.store(cap.height, Ordering::Relaxed);
    *hub.stripes.lock() = vec![(0, cap.height)];
    hub.activity.reset(cap.width, cap.height);
    let mut extra = hub.hello_extra.lock();
    extra.insert("encoderBackend".into(), "nvenc".into());
    extra.insert("captureBuffer".into(), "dmabuf".into());
    drop(extra);
    hub.restart_clients();
    log::info!(
        "GPU video: {}x{}, DMA-BUF -> OpenGL -> NVENC H.264",
        cap.width,
        cap.height
    );
}
fn terminate(hub: &Hub, error: &str) {
    log::error!("GPU capture stopped: {error}");
    if let Some(tx) = hub.input.lock().as_ref() {
        let _ = tx.send(crate::input::InputEvent::Shutdown);
    }
    std::thread::sleep(Duration::from_millis(200));
    std::process::exit(1);
}
fn run(cap: &mut Capturer, cfg: &VideoConfig, hub: &Hub) -> Result<(), String> {
    let quality = crate::wayland::settings().gpu_quality;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Match the software path's jitter allowance: a rigid minimum interval
    // aliases a 60 Hz source into periodic waits/drops as the two clocks drift.
    let min_interval = interval.mul_f64(0.85);
    let mut last = Instant::now() - interval;
    let mut id = 0u32;
    let mut encoded = Vec::new();
    let mut rate = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
    let mut last_motion = Instant::now();
    let mut paintover_left = 0;
    hub.target_kbps
        .store(cfg.max_kbps.min(8000), Ordering::Relaxed);
    loop {
        if hub.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        cap.pump(0)?;
        if cap.resized() {
            return Ok(());
        }
        hub.drain_wake();
        if !cap.changes.is_empty() {
            hub.activity.note(&std::mem::take(&mut cap.changes));
        }
        let snapshots = std::mem::take(&mut *hub.snapshots.lock());
        if !snapshots.is_empty() {
            let shot = Arc::new(crate::record::Snapshot {
                width: cap.width as usize,
                height: cap.height as usize,
                bgrx: cap.gpu.snapshot(cap.current)?,
                seq: hub.activity.seq(),
            });
            for tx in snapshots {
                let _ = tx.send(shot.clone());
            }
        }
        let clients = hub.video_clients();
        let key = clients
            .iter()
            .any(|c| c.needs_keyframe.load(Ordering::Relaxed));
        let paintover = !cap.changed
            && !key
            && paintover_left > 0
            && last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms);
        if clients.is_empty() || (!cap.changed && !key && !paintover) {
            let delay = if paintover_left == 0 {
                100
            } else {
                cfg.paintover_delay_ms
                    .saturating_sub(last_motion.elapsed().as_millis() as u64)
                    .min(100) as i32
                    + 1
            };
            wait_any(&[cap.fd(), hub.waker.waitable()], delay);
            continue;
        }
        rate.update(hub, &clients);
        if !flow_ok(hub, interval.as_secs_f64() * 1000.0) {
            hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
            wait_any(&[cap.fd(), hub.waker.waitable()], 5);
            continue;
        }
        if last.elapsed() < min_interval {
            let ms = (min_interval - last.elapsed()).as_millis().max(1) as i32;
            wait_any(&[cap.fd(), hub.waker.waitable()], ms);
            continue;
        }
        let origin = if cap.changed {
            cap.changed_at
        } else {
            Instant::now()
        };
        let start = Instant::now();
        if cap.changed {
            hub.stats.captured.fetch_add(1, Ordering::Relaxed);
            hub.stats
                .damage_wait_us
                .fetch_add(origin.elapsed().as_micros() as u64, Ordering::Relaxed);
            last_motion = start;
            paintover_left = cfg.paintover_burst;
        } else if paintover {
            paintover_left -= 1;
            hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
        }
        let keyframe = cap.gpu.encode(
            cap.current,
            key,
            hub.target_kbps.load(Ordering::Relaxed),
            if paintover {
                cfg.paintover_crf.clamp(1.0, 51.0)
            } else {
                quality
            },
            &mut encoded,
        )?;
        cap.changed = false;
        last = start;
        if keyframe {
            for c in &clients {
                c.needs_keyframe.store(false, Ordering::Relaxed);
            }
        }
        id = id.wrapping_add(1);
        let len = encoded.len();
        let mut message = Vec::with_capacity(16 + len);
        message.resize(16, 0);
        message[0] = 1;
        message[1] = u8::from(keyframe) | if paintover { 2 } else { 0 };
        message[2..6].copy_from_slice(&id.to_be_bytes());
        message[6..10].copy_from_slice(
            &((origin.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32).to_be_bytes(),
        );
        message[11] = 1;
        message[12] = 1;
        message.extend_from_slice(&encoded);
        let message = Bytes::from(message);
        let sent = Instant::now();
        for c in clients {
            if !c.started.load(Ordering::Relaxed) {
                if !keyframe {
                    continue;
                }
                c.started.store(true, Ordering::Relaxed);
                c.inflight.lock().clear();
            }
            c.inflight.lock().push_back(Inflight {
                id,
                sent,
                bytes: len,
            });
            c.send(Out::Bin(message.clone()));
        }
        hub.stats
            .encode_us
            .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(len as u64, Ordering::Relaxed);
        hub.stats
            .send_lat_us
            .fetch_add(origin.elapsed().as_micros() as u64, Ordering::Relaxed);
        if keyframe {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
    }
}
