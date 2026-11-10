{
  lib,
  rustPlatform,
  pkg-config,
  SDL2,
  libopus,
  ffmpeg_8-full,
  nv-codec-headers-12,
  addDriverRunpath,
  makeWrapper,
  aizomeRenderer ? null,
}:
rustPlatform.buildRustPackage {
  pname = "voxl-viewer";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./build.rs
      ./src
      ../src/retained.rs
      ../media/Cargo.toml
      ../media/build.rs
      ../media/src
      ../media/c
    ];
  };
  cargoRoot = "native";
  buildAndTestSubdir = "native";
  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [ pkg-config addDriverRunpath ] ++ lib.optional (aizomeRenderer != null) makeWrapper;
  buildInputs = [ SDL2 libopus ffmpeg_8-full nv-codec-headers-12 ];
  postFixup = ''
    addDriverRunpath "$out/bin/voxl-viewer"
  '' + lib.optionalString (aizomeRenderer != null) ''
    wrapProgram "$out/bin/voxl-viewer" --set-default AIZOME_REMOTE_RENDERER ${aizomeRenderer}/lib/libaizome_remote_renderer.so
  '';
  # Keep the deterministic scene available to the component benchmark suite.
  meta = {
    description = "Native Wayland Voxl viewer with CPU and CUDA/OpenGL video paths";
    license = lib.licenses.mit;
    mainProgram = "voxl-viewer";
    platforms = [ "x86_64-linux" ];
  };
}
