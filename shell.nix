{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    # Rust toolchain
    rustc
    cargo
    rustfmt
    clippy
    
    # Build essentials
    pkg-config
    
    # Required libraries for Smithay/Anvil
    libxkbcommon
    wayland
    wayland-protocols
    libinput
    libglvnd
    mesa
    udev
    systemd
    seatd
    
    # X11 libraries (for X11 backend support)
    xorg.libX11
    xorg.libxcb
    
    # Additional dependencies
    libdrm
    libgbm
    pixman
    libdisplay-info
  ];

  buildInputs = with pkgs; [
    # Runtime dependencies
    wayland
    libxkbcommon
    libinput
    mesa
    udev
    seatd
  ];

  # Environment variables
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  
  # Wayland specific environment
  XDG_RUNTIME_DIR = "/run/user/1000";
  
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
    pkgs.wayland
    pkgs.libxkbcommon
    pkgs.libinput
    pkgs.libglvnd
    pkgs.mesa
    pkgs.udev
    pkgs.systemd
    pkgs.seatd
    pkgs.libdrm
    pkgs.libgbm
    pkgs.xorg.libX11
    pkgs.xorg.libxcb
  ];
  
  # PKG_CONFIG_PATH for finding .pc files
  PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" [
    pkgs.wayland
    pkgs.wayland-protocols
    pkgs.libxkbcommon
    pkgs.libinput
    pkgs.libglvnd
    pkgs.mesa
    pkgs.udev
    pkgs.systemd
    pkgs.seatd
    pkgs.libdrm
    pkgs.xorg.libX11
    pkgs.xorg.libxcb
  ];
  
  shellHook = ''
    echo "Alice WM development environment"
    echo "======================================"
    echo ""
    echo "To build Alice:"
    echo "  cargo build --release"
    echo ""
    echo "To run Alice (requires appropriate permissions):"
    echo "  cargo run --release"
    echo ""
    echo "Note: Running a Wayland compositor may require:"
    echo "  - Being in the 'input' and 'video' groups"
    echo "  - Access to /dev/dri and /dev/input devices"
    echo "  - Setting XDG_RUNTIME_DIR if not already set"
    echo ""
  '';
}

