fn main() {
    let mut c = cc::Build::new();
    c.file("c/media.c")
        .opt_level(2)
        .warnings(true)
        .flag_if_supported("-std=c11");
    let mut paths = Vec::new();
    let libraries = ["libavcodec", "libavutil", "ffnvcodec"];
    for name in libraries {
        let lib = pkg_config::probe_library(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        c.includes(&lib.include_paths);
        paths.extend(lib.link_paths);
    }
    c.compile("voxl_media");
    for path in &paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
    }
    println!(
        "cargo:rpath={}",
        std::env::join_paths(paths).unwrap().to_string_lossy()
    );
    println!("cargo:rerun-if-changed=c/media.c");
}
