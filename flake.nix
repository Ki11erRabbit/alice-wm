{
  description = "alice-wm — a Smithay-based Wayland compositor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    let
      # Libraries alice-wm links against / dlopens at runtime.
      runtimeDeps = pkgs: with pkgs; [
        wayland
        wayland-protocols
        libxkbcommon
        libinput
        libglvnd
        mesa
        udev
        systemd
        seatd
        libdrm
        libgbm
        pixman
        libdisplay-info

        # X11 libs, for Xwayland support
        xorg.libX11
        xorg.libxcb
      ];

      # Build-time-only tooling (pkg-config, bindgen's libclang, etc.)
      nativeDeps = pkgs: with pkgs; [
        pkg-config
        makeWrapper
      ];
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        alice-wm = pkgs.rustPlatform.buildRustPackage {
          pname = "alice-wm";
          version = "0.1.0";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = nativeDeps pkgs;
          buildInputs = runtimeDeps pkgs;

          # Several of Smithay's deps (drm-rs, gbm-rs, input-rs...) use
          # bindgen at build time, which needs libclang.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          postInstall = ''
            install -Dm644 ${./alice-wm.desktop} \
              $out/share/wayland-sessions/alice-wm.desktop
          '';

          # Wrap so dynamically-loaded libs (EGL/GBM/DRM drivers, libinput
          # backends, etc.) are found at runtime, not just link time.
          postFixup = ''
            wrapProgram $out/bin/alice-wm \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (runtimeDeps pkgs)}
          '';

          # services.displayManager.sessionPackages requires packages to
          # declare which session(s) they ship, matching the DesktopNames
          # in the .desktop file installed above. Without this, NixOS
          # rejects the package with "not of type `package with provided
          # sessions'".
          passthru.providedSessions = [ "alice-wm" ];

          meta = with pkgs.lib; {
            description = "A Smithay-based Wayland compositor";
            homepage = "https://github.com/you/alice-wm";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "alice-wm";
          };
        };
      in
      {
        packages.default = alice-wm;
        packages.alice-wm = alice-wm;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ rustToolchain pkgs.rustfmt pkgs.clippy ] ++ nativeDeps pkgs;
          buildInputs = runtimeDeps pkgs;

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeDeps pkgs);

          PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" (runtimeDeps pkgs);

          # Convenience default for interactive dev sessions; NixOS
          # sessions set the real one via pam/logind, this is just a
          # sane fallback matching the original shell.nix.
          XDG_RUNTIME_DIR = "/run/user/1000";

          shellHook = ''
            echo "alice-wm development environment"
            echo "======================================"
            echo ""
            echo "To build alice-wm:"
            echo "  cargo build --release"
            echo ""
            echo "To run alice-wm (requires appropriate permissions):"
            echo "  cargo run --release"
            echo ""
            echo "Note: Running a Wayland compositor may require:"
            echo "  - Being in the 'input' and 'video' groups"
            echo "  - Access to /dev/dri and /dev/input devices"
            echo "  - Setting XDG_RUNTIME_DIR if not already set"
            echo ""
          '';
        };
      }
    ) // {
      # Non-per-system outputs: the NixOS module.
      nixosModules.default = import ./module.nix { flake = self; };
      nixosModules.alice-wm = self.nixosModules.default;
    };
}
