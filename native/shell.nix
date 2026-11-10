{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ rustc cargo clippy rustfmt pkg-config clang ];
  buildInputs = with pkgs; [ SDL2 ffmpeg_8-full libGL libdrm libgbm nv-codec-headers-12 wayland wayland-protocols ];
}
