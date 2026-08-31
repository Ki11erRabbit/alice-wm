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
      # Runtime deps every Smithay/wayland compositor typically needs.
      runtimeDeps = pkgs: with pkgs; [
        wayland
        libxkbcommon
        libinput
        mesa
        libGL
        seatd
        udev
        systemdLibs
        vulkan-loader
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

          nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
          buildInputs = runtimeDeps pkgs;

          postInstall = ''
            install -Dm644 ${./alice-wm.desktop} \
              $out/share/wayland-sessions/alice-wm.desktop
          '';

          # Wrap so dynamically-loaded libs (EGL/Vulkan drivers, libinput
          # backends, etc.) are found at runtime, not just link time.
          postFixup = ''
            wrapProgram $out/bin/alice-wm \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (runtimeDeps pkgs)}
          '';

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
          nativeBuildInputs = [ rustToolchain pkgs.pkg-config ];
          buildInputs = runtimeDeps pkgs;
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeDeps pkgs);
        };
      }
    ) // {
      # Non-per-system outputs: the NixOS module.
      nixosModules.default = import ./module.nix { flake = self; };
      nixosModules.alice-wm = self.nixosModules.default;
    };
}

