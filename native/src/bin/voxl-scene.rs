//! Deterministic native benchmark surface. The marker is sampled from the
//! receiving compositor, independently of either viewer's decoder/presenter.
use glow::HasContext;
use std::time::{Duration, Instant};

fn monotonic_ms() -> u32 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) },
        0
    );
    (t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000) as u32
}
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let num = |i: usize, default| {
        args.get(i)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default)
    };
    let (w, h, hz, seconds) = (
        num(0, 1920),
        num(1, 1080),
        num(2, 60).clamp(1, 1000),
        num(3, 60),
    );
    let workload = match args.get(4).map(String::as_str).unwrap_or("scroll") {
        "scroll" => 0,
        "animation" => 1,
        "drag" => 2,
        "idle" => 3,
        _ => return Err("workload must be scroll, animation, drag or idle".into()),
    };
    let freeze_after = args
        .iter()
        .find_map(|s| s.strip_prefix("--freeze-after="))
        .map(|s| s.parse::<u32>().map_err(|_| "invalid freeze frame"))
        .transpose()?;
    let marker_cell = args
        .iter()
        .find_map(|s| s.strip_prefix("--marker-cell="))
        .map(|s| s.parse::<u32>().map_err(|_| "invalid marker size"))
        .transpose()?
        .unwrap_or(8);
    if !(8..=32).contains(&marker_cell) || 16 + 56 * marker_cell > w {
        return Err("marker must fit the scene, with cells 8..32 pixels wide".into());
    }
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    video
        .gl_attr()
        .set_context_profile(sdl2::video::GLProfile::Core);
    video.gl_attr().set_context_version(3, 3);
    let window = video
        .window("Voxl benchmark scene", w, h)
        .opengl()
        .fullscreen_desktop()
        .build()
        .map_err(|e| e.to_string())?;
    let _context = window.gl_create_context()?;
    video.gl_set_swap_interval(0)?;
    let gl = unsafe {
        glow::Context::from_loader_function(|s| video.gl_get_proc_address(s) as *const _)
    };
    let program = unsafe {
        let p = gl.create_program()?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, "#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0,1);}"),
            (glow::FRAGMENT_SHADER, r#"#version 330 core
out vec4 color;
uniform uint stamp, frame_id, checksum;
uniform int height, width, mode, marker_cell;
vec3 document(ivec2 p) {
    int row=p.y/24, dy=p.y%24;
    if(row%17==0) return vec3(.18,.44,.82);
    bool ink=dy>4 && dy<18 && p.x>40 && p.x<40+(row*7919)%(width-200)
        && (p.x/9+row*31)%7!=0 && p.x%9<7 && (p.x*13+p.y*7+row)%5<3;
    return ink?vec3(.12,.12,.12):vec3(.96);
}
void main(){
    ivec2 p=ivec2(gl_FragCoord.x,float(height)-gl_FragCoord.y);
    int f=int(frame_id);
    vec3 c;
    if(mode==1) {
        vec2 v=vec2(p)/vec2(width,height);
        c=.5+.5*cos(vec3(3,5,7)*v.x+vec3(7,3,4)*v.y+float(f)*.09+vec3(0,2,4));
        c*=.7+.3*cos(float((p.x+p.y+f*3)%80)*.08);
    } else {
        c=document(ivec2(p.x,p.y+(mode==0?f*6:0)));
        int bx=(f*8)%(width-240), by=height/2-100;
        if(mode==2 && p.x>bx && p.x<bx+240 && p.y>by && p.y<by+200)
            c=document(ivec2(p.x-bx+40,p.y-by));
    }
    if(p.x>=16 && p.x<16+56*marker_cell && p.y>=16 && p.y<16+2*marker_cell){
        int bit=(p.x-16)/marker_cell;
        uint v=bit<32?((stamp>>uint(bit))&1u):bit<48?((frame_id>>uint(bit-32))&1u):((checksum>>uint(bit-48))&1u);
        c=vec3(float(v));
    }
    color=vec4(c,1);
}"#),
        ] {
            let s = gl.create_shader(kind)?;
            gl.shader_source(s, source); gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) { return Err(gl.get_shader_info_log(s)); }
            gl.attach_shader(p, s); gl.delete_shader(s);
        }
        gl.link_program(p);
        if !gl.get_program_link_status(p) {
            return Err(gl.get_program_info_log(p));
        }
        gl.use_program(Some(p));
        let vao = gl.create_vertex_array()?;
        gl.bind_vertex_array(Some(vao));
        p
    };
    let mut events = sdl.event_pump()?;
    let start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / hz as f64);
    let mut next = start;
    let mut frames = 0u32;
    while start.elapsed() < Duration::from_secs(seconds as u64) {
        if events
            .poll_iter()
            .any(|e| matches!(e, sdl2::event::Event::Quit { .. }))
        {
            break;
        }
        if (workload == 3 && frames > 0) || freeze_after.is_some_and(|n| frames >= n) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let stamp = monotonic_ms();
        let fid = frames & 0xffff;
        let check = stamp
            .to_le_bytes()
            .into_iter()
            .chain((fid as u16).to_le_bytes())
            .fold(0xa5, |a, b| a ^ b);
        let (w, h) = window.drawable_size();
        unsafe {
            gl.viewport(0, 0, w as i32, h as i32);
            for (name, value) in [
                ("stamp", stamp),
                ("frame_id", fid),
                ("checksum", check as u32),
            ] {
                gl.uniform_1_u32(gl.get_uniform_location(program, name).as_ref(), value);
            }
            for (name, value) in [
                ("width", w as i32),
                ("height", h as i32),
                ("mode", workload),
                ("marker_cell", marker_cell as i32),
            ] {
                gl.uniform_1_i32(gl.get_uniform_location(program, name).as_ref(), value);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        window.gl_swap_window();
        frames += 1;
        next += interval;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else if next.elapsed() > interval {
            next = Instant::now();
        }
    }
    println!(
        "{}",
        serde_json::json!({"frames":frames,"seconds":start.elapsed().as_secs_f64(),"workload":workload})
    );
    Ok(())
}
