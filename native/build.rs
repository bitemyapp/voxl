fn main() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,/run/opengl-driver/lib");
    for name in ["sdl2", "libavcodec", "libavutil", "opus"] {
        let lib = pkg_config::probe_library(name).unwrap();
        for path in lib.link_paths {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
        }
    }
}
