{
  pkgs ? import <nixpkgs> { },
}:
with pkgs;
let

  libraries = [
    cairo
    dbus
    libGL
    libdisplay-info
    libinput
    seatd
    libxkbcommon
    libgbm
    pango
    wayland
    vulkan-loader
    wayland-protocols
  ];

in
mkShell {
  nativeBuildInputs = with pkgs; [
    cmake
    git
    ninja
    pkg-config
    wayland
    wayland-protocols
    libxkbcommon
    vulkan-loader
    glslang
    clang
    lld
  ];
  buildInputs = libraries;

  shellHook = ''
    export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath libraries}:$LD_LIBRARY_PATH
  '';
}
