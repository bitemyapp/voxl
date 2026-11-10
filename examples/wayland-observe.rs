//! Read only the benchmark marker from a private receiving compositor. This
//! measures compositor-visible content age, not physical scanout or photon time.
#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs::File,
        os::fd::{AsFd, FromRawFd},
        time::{Duration, Instant},
    };
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
    use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols_wlr::screencopy::v1::client::{
        zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager,
    };

    #[derive(Default)]
    struct App {
        info: Option<(wl_shm::Format, u32, u32, u32)>,
        ready: bool,
        failed: bool,
        inverted: bool,
    }
    pub fn run() {
        let seconds = std::env::args()
            .nth(1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10);
        let cell = std::env::args()
            .nth(2)
            .map(|s| s.parse::<u32>().unwrap())
            .unwrap_or(8);
        assert!((8..=32).contains(&cell));
        let conn = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
        let qh = queue.handle();
        let output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let manager: manager::ZwlrScreencopyManagerV1 = globals.bind(&qh, 3..=3, ()).unwrap();
        let mut app = App::default();
        let mut storage = None;
        let start = Instant::now();
        let mut last = None;
        let mut samples = Vec::new();
        let mut invalid = 0;
        let mut captures = 0;
        while start.elapsed() < Duration::from_secs(seconds) {
            app.ready = false;
            app.failed = false;
            app.info = None;
            app.inverted = false;
            let frame = manager.capture_output_region(
                0,
                &output,
                16,
                16,
                (56 * cell) as i32,
                (2 * cell) as i32,
                &qh,
                (),
            );
            while app.info.is_none() && !app.failed {
                queue.blocking_dispatch(&mut app).unwrap();
            }
            assert!(!app.failed, "capture failed");
            let info = app.info.unwrap();
            assert!(matches!(
                info.0,
                wl_shm::Format::Xrgb8888
                    | wl_shm::Format::Argb8888
                    | wl_shm::Format::Xbgr8888
                    | wl_shm::Format::Abgr8888
            ));
            assert_eq!(
                (info.1, info.2),
                (56 * cell, 2 * cell),
                "benchmark requires an unscaled, fullscreen viewer"
            );
            if storage.is_none() {
                let fd =
                    unsafe { libc::memfd_create(c"voxl-observer".as_ptr(), libc::MFD_CLOEXEC) };
                assert!(fd >= 0);
                let file = unsafe { File::from_raw_fd(fd) };
                let size = (info.3 * info.2) as usize;
                file.set_len(size as u64).unwrap();
                let map = unsafe { memmap2::MmapOptions::new().len(size).map(&file) }.unwrap();
                let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
                let buffer = pool.create_buffer(
                    0,
                    info.1 as i32,
                    info.2 as i32,
                    info.3 as i32,
                    info.0,
                    &qh,
                    (),
                );
                pool.destroy();
                storage = Some((map, buffer, info));
            }
            let (map, buffer, expected) = storage.as_ref().unwrap();
            assert_eq!(info, *expected);
            frame.copy(buffer);
            while !app.ready && !app.failed {
                queue.blocking_dispatch(&mut app).unwrap();
            }
            assert!(!app.failed, "marker capture failed");
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
                0
            );
            let now = time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000;
            let mut word = 0u64;
            let y = if app.inverted { cell - 1 } else { cell } as usize;
            for bit in 0..56 {
                let p = y * info.3 as usize + (bit * cell as usize + cell as usize / 2) * 4;
                if u32::from(map[p]) + u32::from(map[p + 1]) + u32::from(map[p + 2]) > 382 {
                    word |= 1 << bit;
                }
            }
            captures += 1;
            let bytes = word.to_le_bytes();
            let check = bytes[..6].iter().fold(0xa5, |a, b| a ^ b);
            let stamp = word as u32;
            let fid = ((word >> 32) & 0xffff) as u16;
            let age = (now as u32).wrapping_sub(stamp);
            if bytes[6] != check || age > 10000 {
                invalid += 1;
            } else if last != Some((stamp, fid)) {
                last = Some((stamp, fid));
                samples.push(serde_json::json!({"frame":fid,"age_ms":age,"observed_ms":now,"elapsed_ms":start.elapsed().as_millis()}));
            }
            frame.destroy();
            // Fixed observation rate, identical for browser/native/local controls.
            std::thread::sleep(Duration::from_millis(2));
        }
        println!(
            "{}",
            serde_json::json!({"samples":samples,"captures":captures,"invalid":invalid,"seconds":start.elapsed().as_secs_f64()})
        );
    }
    impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for App {
        fn event(
            a: &mut Self,
            _: &frame::ZwlrScreencopyFrameV1,
            e: frame::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match e {
                frame::Event::Buffer {
                    format: WEnum::Value(format),
                    width,
                    height,
                    stride,
                } => a.info = Some((format, width, height, stride)),
                frame::Event::Flags {
                    flags: WEnum::Value(flags),
                } => a.inverted = flags.contains(frame::Flags::YInvert),
                frame::Event::Ready { .. } => a.ready = true,
                frame::Event::Failed => a.failed = true,
                _ => {}
            }
        }
    }
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(App:ignore wl_output::WlOutput);
    delegate_noop!(App:ignore wl_shm::WlShm);
    delegate_noop!(App:ignore wl_shm_pool::WlShmPool);
    delegate_noop!(App:ignore wl_buffer::WlBuffer);
    delegate_noop!(App:ignore manager::ZwlrScreencopyManagerV1);
}
fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
