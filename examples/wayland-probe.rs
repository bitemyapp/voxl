//! Native Wayland surface for bench/aizome-smoke.py. It paints blue, turns green
//! on Space, and prints the input actually delivered by the compositor.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsFd, FromRawFd};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    };
    use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
    use xkbcommon::xkb;

    struct App {
        surface: wl_surface::WlSurface,
        blue: wl_buffer::WlBuffer,
        green: wl_buffer::WlBuffer,
        state: Option<xkb::State>,
    }
    fn buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<App>, color: u32) -> wl_buffer::WlBuffer {
        // Safety: a new anonymous file owned by this fixture.
        let fd = unsafe { libc::memfd_create(c"voxl-probe".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut f = unsafe { File::from_raw_fd(fd) };
        let bytes: Vec<_> = (0..400 * 300).flat_map(|_| color.to_ne_bytes()).collect();
        f.write_all(&bytes).unwrap();
        let pool = shm.create_pool(f.as_fd(), bytes.len() as i32, qh, ());
        let buffer = pool.create_buffer(0, 400, 300, 1600, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        buffer
    }
    pub fn run() {
        let conn = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
        seat.get_keyboard(&qh, ());
        seat.get_pointer(&qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_app_id("voxl-wayland-probe".into());
        top.set_title("Voxl native Wayland probe".into());
        let mut app = App {
            surface,
            blue: buffer(&shm, &qh, 0x002040c0),
            green: buffer(&shm, &qh, 0x0020c040),
            state: None,
        };
        app.surface.commit();
        loop {
            queue.blocking_dispatch(&mut app).unwrap();
        }
    }
    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
        fn event(
            _: &mut Self,
            wm: &xdg_wm_base::XdgWmBase,
            e: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = e {
                wm.pong(serial);
            }
        }
    }
    impl Dispatch<xdg_surface::XdgSurface, ()> for App {
        fn event(
            a: &mut Self,
            xdg: &xdg_surface::XdgSurface,
            e: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = e {
                xdg.ack_configure(serial);
                a.surface.attach(Some(&a.blue), 0, 0);
                a.surface.damage_buffer(0, 0, 400, 300);
                a.surface.commit();
                println!("{}", serde_json::json!({"configured":true}));
            }
        }
    }
    impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
        fn event(
            a: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            e: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match e {
                wl_keyboard::Event::Keymap { fd, size, .. } => {
                    let c = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                    // Safety: a compositor-supplied keymap descriptor and size.
                    let map = unsafe {
                        xkb::Keymap::new_from_fd(
                            &c,
                            fd,
                            size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkb::KEYMAP_COMPILE_NO_FLAGS,
                        )
                    }
                    .unwrap()
                    .unwrap();
                    a.state = Some(xkb::State::new(&map));
                }
                wl_keyboard::Event::Modifiers {
                    mods_depressed,
                    mods_latched,
                    mods_locked,
                    group,
                    ..
                } => {
                    if let Some(s) = &mut a.state {
                        s.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    }
                }
                wl_keyboard::Event::Key {
                    key,
                    state: WEnum::Value(state),
                    ..
                } => {
                    let text = a
                        .state
                        .as_ref()
                        .map(|s| s.key_get_utf8(xkb::Keycode::new(key + 8)))
                        .unwrap_or_default();
                    println!(
                        "{}",
                        serde_json::json!({"key":key, "down":state == wl_keyboard::KeyState::Pressed, "text":text})
                    );
                    if key == 57 && state == wl_keyboard::KeyState::Pressed {
                        a.surface.attach(Some(&a.green), 0, 0);
                        a.surface.damage_buffer(0, 0, 400, 300);
                        a.surface.commit();
                    }
                }
                _ => {}
            }
        }
    }
    impl Dispatch<wl_pointer::WlPointer, ()> for App {
        fn event(
            _: &mut Self,
            _: &wl_pointer::WlPointer,
            e: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match e {
                wl_pointer::Event::Motion {
                    surface_x,
                    surface_y,
                    ..
                } => println!("{}", serde_json::json!({"motion":[surface_x,surface_y]})),
                wl_pointer::Event::Button {
                    button,
                    state: WEnum::Value(state),
                    ..
                } => println!(
                    "{}",
                    serde_json::json!({"button":button, "down":state == wl_pointer::ButtonState::Pressed})
                ),
                wl_pointer::Event::Axis { value, .. } => {
                    println!("{}", serde_json::json!({"scroll":value}))
                }
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
    delegate_noop!(App: ignore wl_compositor::WlCompositor);
    delegate_noop!(App: ignore wl_shm::WlShm);
    delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(App: ignore wl_buffer::WlBuffer);
    delegate_noop!(App: ignore wl_surface::WlSurface);
    delegate_noop!(App: ignore wl_seat::WlSeat);
    delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
