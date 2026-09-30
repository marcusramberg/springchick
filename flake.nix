{
  description = "springchick — iOS Springboard-style Wayland compositor";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    let
      overlay = final: prev: {
        springchick = final.callPackage ./nix/package.nix {
          src = self;
          rustPlatform = final.makeRustPlatform {
            cargo = final.springchickRustToolchain;
            rustc = final.springchickRustToolchain;
          };
        };
        springchickRustToolchain = final.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      };
    in
    {
      overlays.default = nixpkgs.lib.composeManyExtensions [
        rust-overlay.overlays.default
        overlay
      ];
      nixosModules.springchick = import ./nix/module.nix { inherit self; };
      nixosModules.default = self.nixosModules.springchick;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            rust-overlay.overlays.default
            overlay
          ];
        };
        # llvm-tools-preview is added here, not in rust-toolchain.toml, so the
        # package derivation (and every VM check) doesn't rebuild.
        # `.override` replaces the toolchain file's components, so rust-analyzer must
        # be repeated. rust-src is required: without it rust-analyzer reports phantom
        # E0308s on every unsize coercion.
        rust =
          (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
            extensions = [
              "llvm-tools-preview"
              "rust-analyzer"
              "rust-src"
            ];
          };
      in
      {
        packages.springchick = pkgs.springchick;
        packages.default = pkgs.springchick;

        # VM tests: `nix build .#checks.<system>.vm-boot -L`. Build the check for
        # `builtins.currentSystem`; cross-building under qemu-user crashes rustc.
        checks = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm-boot = import ./nix/vm-test.nix { inherit self pkgs; };
          vm-switcher = import ./nix/vm-switcher-test.nix { inherit self pkgs; };
          vm-dialog = import ./nix/vm-dialog-test.nix { inherit self pkgs; };
          vm-rotation = import ./nix/vm-rotation-test.nix { inherit self pkgs; };
          vm-arrange = import ./nix/vm-arrange-test.nix { inherit self pkgs; };
          vm-icon-menu = import ./nix/vm-icon-menu-test.nix { inherit self pkgs; };
          vm-library = import ./nix/vm-library-test.nix { inherit self pkgs; };
          vm-lock = import ./nix/vm-lock-test.nix { inherit self pkgs; };
          vm-capture = import ./nix/vm-capture-test.nix { inherit self pkgs; };
          vm-portal = import ./nix/vm-portal-test.nix { inherit self pkgs; };
          vm-pointer = import ./nix/vm-pointer-test.nix { inherit self pkgs; };
          vm-card-mask = import ./nix/vm-card-mask-test.nix { inherit self pkgs; };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rust
            pkgs.pkg-config
            pkgs.wayland
            pkgs.libinput
            pkgs.libxkbcommon
            pkgs.libGL
            pkgs.mesa
            pkgs.udev
            pkgs.seatd
            # iio-sensor-proxy client.
            pkgs.dbus
            pkgs.libgbm
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxi
            pkgs.fontconfig
            pkgs.freetype
            pkgs.clang
            pkgs.python3
            pkgs.cargo-llvm-cov
            pkgs.just
            # packaging/build.sh: one manifest → apk/deb/pkg.tar.zst.
            pkgs.nfpm
          ];
          # bindgen for skia-safe needs libclang. winit/EGL dlopen their libs at
          # runtime, hence LD_LIBRARY_PATH.
          shellHook = ''
            export RUST_BACKTRACE=1
            # ~/.cargo/bin rustup shims can land ahead in PATH and die trying to download
            # a toolchain. Make the pinned one win.
            export PATH="${rust}/bin:$PATH"
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
            export LD_LIBRARY_PATH="${
              pkgs.lib.makeLibraryPath [
                pkgs.wayland
                pkgs.libxkbcommon
                pkgs.libGL
                pkgs.mesa
                pkgs.libgbm
              ]
            }''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            # There's no rustup; these must match the rustc that built the binaries.
            export LLVM_COV="${rust}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin/llvm-cov"
            export LLVM_PROFDATA="${rust}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin/llvm-profdata"
          '';
        };
      }
    );
}
