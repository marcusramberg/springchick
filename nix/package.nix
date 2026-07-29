{
  lib,
  stdenv,
  rustPlatform,
  fetchurl,
  makeWrapper,
  pkg-config,
  python3,
  wayland,
  wayland-scanner,
  libxkbcommon,
  libinput,
  libgbm,
  libGL,
  mesa,
  udev,
  seatd,
  fontconfig,
  freetype,
  expat,
  zlib,
  xwayland,
  src ? ../.,
  version ? "0.1.0",
}:

let
  # skia-bindings' build script normally downloads a prebuilt Skia from GitHub,
  # which the Nix sandbox forbids. Fetch the exact archive up front and point
  # SKIA_BINARIES_URL at it with a file:// URL (the build script special-cases
  # those and reads the file directly, no curl).
  #
  # The archive name is `skia-binaries-<rust-skia repo hash>-<target>-<features>`.
  # Bumping skia-safe changes the repo hash and the version tag below; when that
  # happens, list the assets of the matching release at
  # https://github.com/rust-skia/skia-binaries/releases and update all three
  # values. `<features>` must match the enabled skia-safe features exactly
  # (we build with `gl`, which resolves to `gl-jpegd-jpege-pdf`).
  skiaVersion = "0.99.0";
  skiaRepoHash = "a25a0fdb7d90429aa2d1";
  skiaFeatures = "gl-jpegd-jpege-pdf";

  skiaBinariesHashes = {
    "aarch64-unknown-linux-gnu" = "sha256-VzULtbs9SmgdNX4/2K+Q7aFxfFz49Rdc/Az5d34PZ6o=";
    "x86_64-unknown-linux-gnu" = "sha256-WJ8o/gHlqaxpfRhX1PvCzbw2TlaLyKlZI9EI5M7nu4w=";
  };

  rustTarget = stdenv.hostPlatform.rust.rustcTarget;

  skiaBinaries = fetchurl {
    url = "https://github.com/rust-skia/skia-binaries/releases/download/${skiaVersion}/skia-binaries-${skiaRepoHash}-${rustTarget}-${skiaFeatures}.tar.gz";
    hash =
      skiaBinariesHashes.${rustTarget}
        or (throw "no prebuilt Skia binaries pinned for target ${rustTarget}");
  };

  # winit/EGL/GLES and libseat are dlopen'd at runtime, so they must be on the
  # loader path of the installed binary — build-time rpath does not cover them.
  runtimeLibs = [
    wayland
    libxkbcommon
    libGL
    mesa
    libgbm
    libinput
    udev
    seatd
  ];
in
rustPlatform.buildRustPackage {
  pname = "springchick";
  inherit version src;

  cargoLock = {
    lockFile = ../Cargo.lock;
    # smithay is a fork of ff5fa7d (pinned for xkbcommon 0.9 → wvkbd keymap fix)
    # carrying a one-commit fix for the layer-surface destroy crash (Smithay#1979).
    # git deps must be vendored with an explicit hash.
    outputHashes = {
      "smithay-0.7.0" = "sha256-LmOKXMDqaEkML8e3nN2IThvKBGtBzk3ggsKwLsYyI0o=";
    };
  };

  nativeBuildInputs = [
    pkg-config
    python3
    makeWrapper
    wayland-scanner
  ];

  buildInputs = [
    wayland
    libxkbcommon
    libinput
    libgbm
    libGL
    mesa
    udev
    seatd
    fontconfig
    freetype
    expat
    zlib
  ];

  env.SKIA_BINARIES_URL = "file://${skiaBinaries}";

  # Only the compositor binary is wanted; the other workspace crates are libs.
  cargoBuildFlags = [
    "-p"
    "sc-compositor"
  ];

  # Workspace tests need no display; keep them on so `nix flake check` is useful.
  cargoTestFlags = [ "--workspace" ];

  postInstall = ''
    wrapProgram $out/bin/springchick \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}" \
      --prefix PATH : "${lib.makeBinPath [ xwayland ]}"

    # Session entry point launched by the display manager. It does not exec the
    # compositor directly: it starts springchick.service (Type=notify) so that
    # graphical-session.target is pulled active via the service's BindsTo, which
    # is the only legal way to raise that RefuseManualStart target. See
    # nix/springchick-session and the systemd.user units in nix/module.nix. The
    # DRM backend / XDG_SESSION_TYPE now live on the service, not here.
    install -Dm555 ${./springchick-session} $out/bin/springchick-session
    substituteInPlace $out/bin/springchick-session \
      --replace-fail '@springchick@' "$out/bin/springchick"

    install -Dm444 ${./springchick.desktop} \
      $out/share/wayland-sessions/springchick.desktop

    # Sample config with every option at its built-in default. The module
    # installs this to /etc/springchick/config.toml.example as a starting point.
    install -Dm444 ${../config.example.toml} \
      $out/share/springchick/config.example.toml
  '';

  # Required by services.displayManager.sessionPackages; must match the
  # desktop file's DesktopNames.
  passthru.providedSessions = [ "springchick" ];

  meta = {
    description = "iOS Springboard-style Wayland compositor";
    mainProgram = "springchick";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
