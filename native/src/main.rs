mod audio;
mod input;
mod net;
mod protocol;
mod render;
#[path = "../../src/retained.rs"]
pub mod retained;
mod scene;
mod stream;
use net::{Incoming, Network};
use sdl2::{
    event::{Event, WindowEvent},
    keyboard::{Keycode, Mod},
};
use std::time::{Duration, Instant};
use voxl_media::Decode;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(error) = run() {
        eprintln!("voxl-viewer: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|s| s == "--help") || args.is_empty() {
        println!("Usage: voxl-viewer URL [--decoder=cpu|auto|cuda] [--fullscreen] [--vsync] [--no-audio] [--native-widgets] [--renderer=PATH] [--scene-compression=zlib|none] [--seconds=N]\nVOXL_TOKEN supplies authentication without putting it in the URL. F11 toggles fullscreen; F8 toggles audio; F9 toggles optional Aizome native widgets.");
        return Ok(());
    }
    for arg in &args[1..] {
        if !matches!(
            arg.as_str(),
            "--vsync" | "--fullscreen" | "--no-audio" | "--native-widgets"
        ) && !arg.starts_with("--decoder=")
            && !arg.starts_with("--seconds=")
            && !arg.starts_with("--renderer=")
            && !arg.starts_with("--scene-compression=")
        {
            return Err(format!("unknown option: {arg}"));
        }
    }
    let scene_codec = match args
        .iter()
        .find_map(|s| s.strip_prefix("--scene-compression="))
        .unwrap_or("zlib")
    {
        "zlib" => "zlib",
        "none" => "raw",
        _ => return Err("--scene-compression must be zlib or none".into()),
    };
    let mut scene_transport = retained::Decoder::default();
    let mode = match args
        .iter()
        .find_map(|s| s.strip_prefix("--decoder="))
        .unwrap_or("auto")
    {
        "cpu" => Decode::Cpu,
        "auto" => Decode::Auto,
        "cuda" => Decode::Cuda,
        _ => return Err("unknown decoder".into()),
    };
    let seconds = args
        .iter()
        .find_map(|s| s.strip_prefix("--seconds="))
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| "--seconds must be a nonnegative integer")
        })
        .transpose()?;
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let attrs = video.gl_attr();
    attrs.set_context_profile(sdl2::video::GLProfile::Core);
    attrs.set_context_version(3, 3);
    attrs.set_double_buffer(true);
    let mut window = video
        .window("Voxl — connecting", 1280, 800)
        .opengl()
        .resizable()
        .allow_highdpi()
        .position_centered()
        .build()
        .map_err(|e| e.to_string())?;
    let _context = window.gl_create_context()?;
    if args.iter().any(|s| s == "--fullscreen") {
        window.set_fullscreen(sdl2::video::FullscreenType::Desktop)?;
    }
    let _ = video.gl_set_swap_interval(if args.iter().any(|s| s == "--vsync") {
        1
    } else {
        0
    });
    let mut events = sdl.event_pump()?;
    let event_system = sdl.event()?;
    // A private SDL user event carries no pointers; it only wakes the event wait.
    let wake_type = unsafe { event_system.register_event()? };
    let mut net = Network::connect(&args[0], event_system.event_sender(), wake_type)?;
    video.text_input().start();
    let mut stream: Option<stream::Stream> = None;
    let renderer_path = args
        .iter()
        .find_map(|s| s.strip_prefix("--renderer=").map(str::to_string))
        .or_else(|| std::env::var("AIZOME_REMOTE_RENDERER").ok())
        .unwrap_or_else(|| "libaizome_remote_renderer.so".into());
    let mut scene = match scene::Scene::new(&video, &renderer_path) {
        Ok(scene) => Some(scene),
        Err(error) => {
            log::info!("Optional Aizome renderer unavailable: {error}");
            None
        }
    };
    let mut want_scene = args.iter().any(|s| s == "--native-widgets") && scene.is_some();
    let mut scene_nonce = 0u32;
    let mut scene_reason = String::from("off");
    let mut source = (1280, 800);
    let mut read_only = false;
    let mut connected = false;
    let mut redraw = false;
    let mut frames = 0u64;
    let mut decode_us = 0u128;
    let mut draw_us = 0u128;
    let mut swap_us = 0u128;
    let mut swap_max_us = 0u128;
    let start = Instant::now();
    let mut reported = Instant::now();
    let mut waiting_event = None;
    let mut keyboard = input::Keyboard::default();
    let mut last_key_request = Instant::now() - Duration::from_secs(1);
    let mut decode_mode = mode;
    let mut want_audio = !args.iter().any(|s| s == "--no-audio");
    let mut server_audio = false;
    let mut audio: Option<audio::Playback> = None;
    'running: loop {
        if seconds.is_some_and(|s| start.elapsed() >= Duration::from_secs(s)) {
            break;
        }
        for event in waiting_event.take().into_iter().chain(events.poll_iter()) {
            match event {
                Event::Quit { .. } => break 'running,
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } if connected => {
                    keyboard.clear();
                    net.command("r")?;
                }
                Event::Window {
                    win_event:
                        WindowEvent::Exposed | WindowEvent::Resized(..) | WindowEvent::SizeChanged(..),
                    ..
                } => redraw = true,
                Event::KeyDown {
                    keycode: Some(Keycode::F9),
                    repeat: false,
                    ..
                } => {
                    want_scene = !want_scene && scene.is_some();
                    scene_nonce = scene_nonce.wrapping_add(1);
                    if let Some(s) = scene.as_mut() {
                        s.reset();
                    }
                    if let Some(s) = stream.as_mut() {
                        s.wait_key();
                    }
                    scene_reason = if want_scene { "waiting" } else { "off" }.into();
                    if connected {
                        net.command(format!(
                            "scene,{},{},{}",
                            u8::from(want_scene),
                            scene_nonce,
                            scene_codec
                        ))?;
                        net.command("v,1")?;
                    }
                    redraw = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F8),
                    repeat: false,
                    ..
                } => {
                    want_audio = !want_audio;
                    if let Some(player) = audio.as_mut() {
                        player.reset()?;
                    }
                    if connected {
                        net.command(format!(
                            "au,{}",
                            u8::from(want_audio && server_audio && audio.is_some())
                        ))?;
                    }
                    reported = Instant::now() - Duration::from_secs(1);
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F11),
                    repeat: false,
                    ..
                } => {
                    let full = window.fullscreen_state() == sdl2::video::FullscreenType::Off;
                    window.set_fullscreen(if full {
                        sdl2::video::FullscreenType::Desktop
                    } else {
                        sdl2::video::FullscreenType::Off
                    })?;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F8 | Keycode::F9 | Keycode::F11),
                    ..
                }
                | Event::KeyUp {
                    keycode: Some(Keycode::F8 | Keycode::F9 | Keycode::F11),
                    ..
                } => {}
                Event::MouseMotion { x, y, .. } if connected && !read_only => {
                    let r = render::viewport(window.size(), source);
                    if r[2] > 0 && r[3] > 0 {
                        let x = ((x - r[0]) as i64 * source.0 as i64 / r[2] as i64)
                            .clamp(0, source.0 as i64 - 1);
                        let y = ((y - r[1]) as i64 * source.1 as i64 / r[3] as i64)
                            .clamp(0, source.1 as i64 - 1);
                        net.command(format!("m,{x},{y}"))?;
                    }
                }
                Event::MouseButtonDown { mouse_btn, .. }
                | Event::MouseButtonUp { mouse_btn, .. }
                    if connected && !read_only =>
                {
                    let down = matches!(event, Event::MouseButtonDown { .. });
                    let b = match mouse_btn {
                        sdl2::mouse::MouseButton::Left => 1,
                        sdl2::mouse::MouseButton::Middle => 2,
                        sdl2::mouse::MouseButton::Right => 3,
                        sdl2::mouse::MouseButton::X1 => 8,
                        sdl2::mouse::MouseButton::X2 => 9,
                        _ => 0,
                    };
                    if b != 0 {
                        net.command(format!("b,{b},{}", u8::from(down)))?;
                    }
                }
                Event::MouseWheel {
                    precise_x,
                    precise_y,
                    ..
                } if connected && !read_only => {
                    net.command(format!("sp,{},{}", precise_x * 120.0, -precise_y * 120.0))?
                }
                Event::TextInput { .. } | Event::KeyDown { .. } | Event::KeyUp { .. }
                    if connected && !read_only =>
                {
                    if matches!(&event,Event::KeyDown {keycode:Some(Keycode::V),keymod,repeat:false,..}
                        if keymod.intersects(Mod::LCTRLMOD|Mod::RCTRLMOD))
                    {
                        if let Ok(text) = video.clipboard().clipboard_text() {
                            if text.len() <= 8 * 1024 * 1024 {
                                net.command(format!("c,{text}"))?;
                            }
                        }
                    }
                    for command in keyboard.event(&event) {
                        net.command(command)?;
                    }
                }
                _ => {}
            }
        }
        // Drain only a bounded number: input processing stays responsive under load.
        for _ in 0..4 {
            match net.receive.try_recv() {
                Ok(Incoming::Connected) => {
                    connected = true;
                    net.command("r")?;
                }
                Ok(Incoming::Disconnected(reason)) => {
                    connected = false;
                    if let Some(s) = scene.as_mut() {
                        s.reset();
                    }
                    if let Some(s) = stream.as_mut() {
                        s.wait_key();
                    }
                    redraw = true;
                    keyboard.clear();
                    if let Some(player) = audio.as_mut() {
                        player.reset()?;
                    }
                    log::warn!("Disconnected: {reason}; reconnecting");
                    let _ = window.set_title("Voxl — disconnected, reconnecting");
                }
                Ok(Incoming::Text(text)) => {
                    let m: serde_json::Value =
                        serde_json::from_str(&text).map_err(|e| e.to_string())?;
                    match m["t"].as_str() {
                        Some("hello") => {
                            server_audio = m["audio"].as_bool().unwrap_or(false);
                            if server_audio && audio.is_none() {
                                match audio::Playback::new(&sdl) {
                                    Ok(player) => audio = Some(player),
                                    Err(error) => log::warn!("Audio unavailable: {error}"),
                                }
                            }
                            net.command(format!(
                                "au,{}",
                                u8::from(want_audio && server_audio && audio.is_some())
                            ))?;
                            let hello: protocol::Hello =
                                serde_json::from_value(m).map_err(|e| e.to_string())?;
                            let hello = hello.validate()?;
                            source = (hello.w, hello.h);
                            read_only = hello.read_only;
                            drop(stream.take());
                            let next = stream::Stream::new(&video, hello, decode_mode)?;
                            log::info!(
                                "stream {}x{}, hardware decode={}",
                                source.0,
                                source.1,
                                next.hardware()
                            );
                            stream = Some(next);
                            net.command("v,1")?;
                            scene_nonce = scene_nonce.wrapping_add(1);
                            if let Some(s) = scene.as_mut() {
                                s.reset();
                            }
                            if want_scene {
                                net.command(format!("scene,1,{scene_nonce},{scene_codec}"))?;
                            }
                        }
                        Some("scene")
                            if m["nonce"].as_u64() == Some(u64::from(scene_nonce))
                                && want_scene =>
                        {
                            scene_reason = m["reason"].as_str().unwrap_or("pixel fallback").into();
                            log::info!("Aizome native widgets: {scene_reason}");
                            if let Some(s) = scene.as_mut() {
                                s.reset();
                            }
                            if let Some(s) = stream.as_mut() {
                                s.wait_key();
                            }
                            net.command("v,1")?;
                            redraw = true;
                        }
                        Some("clip") => {
                            if let Some(t) = m["d"].as_str() {
                                let _ = video.clipboard().set_clipboard_text(t);
                            }
                        }
                        Some("close") => return Err("server closed the stream".into()),
                        _ => {}
                    }
                }
                Ok(Incoming::Binary(bytes)) if matches!(bytes.first(), Some(0x20 | 0x21)) => {
                    if bytes.len() < 9 {
                        return Err("truncated retained packet".into());
                    }
                    let nonce = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
                    let serial = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
                    if want_scene && nonce == scene_nonce {
                        if let Some(player) = scene.as_mut() {
                            let was_active = player.active;
                            match scene_transport
                                .packet(&bytes)
                                .and_then(|packet| player.frame(&packet.payload, source))
                            {
                                Ok(()) => {
                                    if !was_active {
                                        net.command("v,0")?;
                                        log::info!("Aizome native widgets active");
                                    }
                                    net.command(format!("sa,{scene_nonce},{serial}"))?;
                                    scene_reason = "native".into();
                                    redraw = true;
                                }
                                Err(error) => {
                                    log::warn!("Aizome replay failed: {error}; using video");
                                    player.reset();
                                    scene_reason = "pixel fallback".into();
                                    // Cancel this failed dictionary. F9 can request a fresh one.
                                    scene_nonce = scene_nonce.wrapping_add(1);
                                    net.command(format!("scene,0,{scene_nonce}"))?;
                                    if let Some(s) = stream.as_mut() {
                                        s.wait_key();
                                    }
                                    net.command("v,1")?;
                                    redraw = true;
                                }
                            }
                        }
                    }
                }
                Ok(Incoming::Binary(bytes)) if bytes.first() == Some(&1) => {
                    let before = Instant::now();
                    let current = stream.as_mut().ok_or("video before hello")?;
                    let old = current.frames;
                    match current.packet(&bytes) {
                        Ok(result) => {
                            for id in result.acks {
                                net.command(format!("a,{id}"))?;
                            }
                            if result.request_key
                                && last_key_request.elapsed() > Duration::from_millis(500)
                            {
                                net.command("kf")?;
                                last_key_request = Instant::now();
                            }
                            let decoded = current.frames - old;
                            redraw |= decoded > 0;
                            frames += decoded;
                        }
                        Err(error) if decode_mode == Decode::Auto && current.hardware() => {
                            log::warn!("GPU decode/presentation failed: {error}; switching to CPU");
                            let hello = current.hello.clone();
                            drop(stream.take());
                            decode_mode = Decode::Cpu;
                            stream = Some(stream::Stream::new(&video, hello, Decode::Cpu)?);
                            net.command("kf")?;
                            last_key_request = Instant::now();
                        }
                        Err(error) => return Err(error),
                    }
                    decode_us += before.elapsed().as_micros();
                }
                Ok(Incoming::Binary(bytes)) if bytes.first() == Some(&2) => {
                    if want_audio {
                        if let Some(player) = audio.as_mut() {
                            if let Err(error) = player.packet(&bytes[1..]) {
                                log::warn!("Audio stopped: {error}");
                                audio = None;
                                net.command("au,0")?;
                            }
                        }
                    }
                }
                Ok(Incoming::Binary(_)) => {}
                Ok(Incoming::Closed(reason)) => return Err(reason),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err("network worker stopped".into())
                }
            }
        }
        if redraw {
            let before = Instant::now();
            if let Some(player) = scene.as_ref().filter(|s| s.active) {
                player.draw(window.drawable_size());
            } else if let Some(current) = stream.as_ref().filter(|s| s.visible()) {
                current.draw(window.drawable_size());
            } else {
                use glow::HasContext;
                unsafe {
                    let gl = glow::Context::from_loader_function(|s| {
                        video.gl_get_proc_address(s) as *const _
                    });
                    gl.clear_color(0.0, 0.0, 0.0, 1.0);
                    gl.clear(glow::COLOR_BUFFER_BIT);
                }
            }
            draw_us += before.elapsed().as_micros();
            let before = Instant::now();
            window.gl_swap_window();
            let took = before.elapsed().as_micros();
            swap_us += took;
            swap_max_us = swap_max_us.max(took);
            redraw = false;
        }
        if connected && reported.elapsed() >= Duration::from_secs(1) {
            let _ = window.set_title(&format!(
                "Voxl — {}x{} · {} decode · {:.0} fps · F8 audio {} · F9 widgets {}",
                source.0,
                source.1,
                if stream.as_ref().is_some_and(|s| s.hardware()) {
                    "GPU"
                } else {
                    "CPU"
                },
                frames as f64 / start.elapsed().as_secs_f64(),
                if want_audio && server_audio && audio.is_some() {
                    "on"
                } else {
                    "off"
                },
                if scene.is_none() {
                    "unavailable"
                } else if want_scene {
                    &scene_reason
                } else {
                    "off"
                }
            ));
            reported = Instant::now();
        }
        waiting_event = events.wait_event_timeout(if net.receive.is_empty() { 250 } else { 0 });
    }
    let _ = net.command("r");
    println!(
        "{}",
        serde_json::json!({"frames":frames,"elapsed_ms":start.elapsed().as_millis(),"decode_upload_us":decode_us,
            "draw_us":draw_us,"swap_us":swap_us,"swap_max_us":swap_max_us,
            "hardware_decode":stream.as_ref().is_some_and(|s|s.hardware()),
            "scene_frames":scene.as_ref().map_or(0,|s|s.frames),
            "scene_bytes":scene.as_ref().map_or(0,|s|s.bytes),
            "scene_paint_upload_us":scene.as_ref().map_or(0,|s|s.paint_us),
            "scene_uploaded_bytes":scene.as_ref().map_or(0,|s|s.uploaded_bytes),
            "audio_packets":audio.as_ref().map_or(0, |a|a.packets),
            "audio_discarded_queues":audio.as_ref().map_or(0, |a|a.discarded_queues)})
    );
    Ok(())
}
