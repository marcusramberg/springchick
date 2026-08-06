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
        # The pinned toolchain, plus `llvm-tools-preview` for `cargo llvm-cov`.
        # The extension is added here rather than in `rust-toolchain.toml` on
        # purpose: coverage is a dev-shell concern, and extending the toolchain
        # the *package* builds with would change its derivation and force every
        # VM check to rebuild the release tree.
        rust =
          (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override
            { extensions = [ "llvm-tools-preview" ]; };
      in
      {
        packages.springchick = pkgs.springchick;
        packages.default = pkgs.springchick;

        # VM tests (nixos test driver). Boot-smoke gates the DRM/GL stack in a
        # headless VM; run with `nix build .#checks.<system>.vm-boot -L`. Built
        # for the host arch (aarch64-linux and x86_64-linux) — always build the
        # check matching `nix eval --raw --impure --expr builtins.currentSystem`,
        # since cross-building the guest under qemu-user emulation crashes rustc.
        checks = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm-boot = import ./nix/vm-test.nix { inherit self pkgs; };
          vm-switcher = import ./nix/vm-switcher-test.nix { inherit self pkgs; };
          vm-dialog = import ./nix/vm-dialog-test.nix { inherit self pkgs; };
          vm-rotation = import ./nix/vm-rotation-test.nix { inherit self pkgs; };
          vm-arrange = import ./nix/vm-arrange-test.nix { inherit self pkgs; };
          vm-lock = import ./nix/vm-lock-test.nix { inherit self pkgs; };
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
            # libdbus: the iio-sensor-proxy client (accelerometer orientation).
            pkgs.dbus
            pkgs.libgbm
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxi
            pkgs.fontconfig
            pkgs.freetype
            pkgs.clang
            pkgs.python3
            # `cargo llvm-cov` — see the coverage note in CONTRIBUTING.md.
            pkgs.cargo-llvm-cov
          ];
          # skia-safe's build script runs bindgen — point it at libclang up front.
          # winit's Wayland backend + EGL/GLES dlopen their libs at RUNTIME; in a nix
          # shell those .so files aren't on the loader path, so expose them via
          # LD_LIBRARY_PATH or you get WaylandError(NoWaylandLib) at startup.
          shellHook = ''
            export RUST_BACKTRACE=1
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
            # cargo-llvm-cov shells out to llvm-profdata/llvm-cov and finds them
            # via rustup by default. There is no rustup here, so point it at the
            # pinned toolchain's own copies — they must match the rustc that
            # produced the instrumented binaries, or the profdata is unreadable.
            export LLVM_COV="${rust}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin/llvm-cov"
            export LLVM_PROFDATA="${rust}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin/llvm-profdata"
          '';
        };
      }
    );
}
