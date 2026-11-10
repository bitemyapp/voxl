{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = with pkgs; [
    (python3.withPackages (p: with p; [ websockets pillow av playwright numpy ]))
    ffmpeg_8-full
    grim
    wl-clipboard
  ];
  # AIZOME_BIN and VOXL_BROWSER select the exact compositor/browser under test.
}
