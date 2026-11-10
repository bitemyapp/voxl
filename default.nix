{
  lib,
  rustPlatform,
  pkg-config,
  x264,
  libyuv,
  libopus,
  libpulseaudio,
  libxkbcommon,
  xorg,
  esbuild,
  typescript,
  withGpu ? false,
  libGL,
  libgbm,
  libdrm,
  nv-codec-headers-12,
  addDriverRunpath,
}:
rustPlatform.buildRustPackage {
  pname = "voxl";
  version = "0.3.4";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./build.rs
      ./src
      ./web
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;
  buildFeatures = lib.optional withGpu "gpu";

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
    esbuild
    typescript
  ] ++ lib.optional withGpu addDriverRunpath;
  postFixup = lib.optionalString withGpu ''
    addDriverRunpath "$out/bin/voxl"
  '';

  # build.rs bundles web/src with esbuild; type-check it first.
  preBuild = ''
    tsc -p web
    tsc -p web/tsconfig.worker.json
    tsc -p web/tsconfig.worklet.json
  '';

  buildInputs = [
    x264
    libyuv
    libopus
    libpulseaudio
    libxkbcommon
    xorg.libxcb
  ] ++ lib.optionals withGpu [ libGL libgbm libdrm nv-codec-headers-12 ];

  meta = {
    description = "Low-latency Aizome Wayland and X11 desktop streaming server (H.264 over WebSocket)";
    license = lib.licenses.mit;
    mainProgram = "voxl";
    platforms = ["x86_64-linux"];
  };
}
