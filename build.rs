fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    #[cfg(feature = "gpu")]
    if target == "linux" {
        gpu_libs();
    }
    let include_paths = if target == "windows" {
        windows_libs()
    } else {
        unix_libs(&target)
    };
    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after the library's build number (a macro
    // bindgen can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    std::fs::write(out.join("x264.rs"), code).unwrap();
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(feature = "gpu")]
fn gpu_libs() {
    let mut c = cc::Build::new();
    c.file("src/gpu/encoder.c")
        .opt_level(2)
        .flag_if_supported("-std=c11");
    for name in ["ffnvcodec", "gbm", "egl", "gl", "libdrm"] {
        let lib = pkg_config::probe_library(name).unwrap_or_else(|e| panic!("GPU {name}: {e}"));
        c.includes(lib.include_paths);
        for path in lib.link_paths {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
        }
    }
    c.compile("voxl_gpu");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/run/opengl-driver/lib");
    println!("cargo:rerun-if-changed=src/gpu/encoder.c");
}

/// x264, libyuv, Opus and PulseAudio from pkg-config; returns x264's include paths.
fn unix_libs(target: &str) -> Vec<std::path::PathBuf> {
    let x264 = pkg_config::probe_library("x264").expect("x264");
    // macOS captures NV12 (no libyuv) and gets audio from ScreenCaptureKit (no PulseAudio).
    let libs: &[&str] = if target == "macos" {
        &["opus"]
    } else {
        &["libyuv", "opus", "libpulse-simple", "xkbcommon"]
    };
    let others: Vec<_> = libs
        .iter()
        .filter_map(|&l| match pkg_config::probe_library(l) {
            // libyuv's CMake install writes no libyuv.pc, so Arch's and Debian's packages
            // have none: link it from the default library path.
            Err(_) if l == "libyuv" => {
                println!("cargo:rustc-link-lib=yuv");
                None
            }
            lib => Some(lib.expect(l)),
        })
        .collect();
    // Embed library dirs so the binary runs outside the build shell. The link args only
    // reach voxl's own binary, so programs that embed voxl get the dirs as DEP_VOXL_RPATH.
    let dirs: Vec<_> = std::iter::once(&x264)
        .chain(others.iter())
        .flat_map(|lib| &lib.link_paths)
        .collect();
    for p in &dirs {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    let rpath = std::env::join_paths(&dirs).expect("library dirs");
    println!("cargo:rpath={}", rpath.to_string_lossy());
    x264.include_paths
}

/// x264, libyuv and Opus from vcpkg (`vcpkg install x264 libyuv opus`, triplet from
/// VCPKGRS_TRIPLET, e.g. x64-windows-static-md for a self-contained voxl.exe); returns
/// x264's include paths. Capture, audio and input use system DLLs.
fn windows_libs() -> Vec<std::path::PathBuf> {
    let x264 = vcpkg::Config::new()
        .find_package("x264")
        .expect("x264 from vcpkg");
    for lib in ["libyuv", "opus"] {
        vcpkg::Config::new()
            .find_package(lib)
            .unwrap_or_else(|e| panic!("{lib} from vcpkg: {e}"));
    }
    println!("cargo:rerun-if-env-changed=VCPKGRS_TRIPLET");
    x264.include_paths
}

/// Bundles the TypeScript web client (web/src) into OUT_DIR/web, which the server
/// embeds. Needs esbuild on PATH (or ESBUILD); shell.nix and default.nix provide it.
fn bundle_web(out: &std::path::Path) {
    // npm installs esbuild on Windows as a .cmd shim.
    let default = if cfg!(windows) {
        "esbuild.cmd"
    } else {
        "esbuild"
    };
    let esbuild = std::env::var("ESBUILD").unwrap_or_else(|_| default.into());
    let status = std::process::Command::new(&esbuild)
        .args([
            "client=web/src/client/main.ts",
            "worker=web/src/worker.ts",
            "audio=web/src/worklets/audio.ts",
            "mic=web/src/worklets/mic.ts",
            "--bundle",
            "--format=iife",
            "--target=es2022",
            "--minify",
            "--log-level=warning",
        ])
        .arg(format!("--outdir={}", out.join("web").display()))
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "running {esbuild} to bundle web/src failed ({e}); use nix-shell, or set ESBUILD"
            )
        });
    assert!(status.success(), "esbuild failed to bundle web/src");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-env-changed=ESBUILD");
}
