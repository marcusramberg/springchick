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
  dbus,
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

  # Only the Rust workspace feeds the build. The flake passes `src = self` (the
  # whole tree), so without this filter an edit to anything — nix/, tests/,
  # docs/, scripts/ — changes the src hash and busts the expensive release
  # build. Restrict to the files cargo actually reads; postInstall pulls
  # config.example.toml and the session scripts via their own nix paths, so they
  # are unaffected.
  cargoSrc = lib.cleanSourceWith {
    inherit src;
    # `src` arrives as a string-like store path (flake `self`), which
    # lib.fileset rejects — cleanSourceWith's path filter accepts it. Keep the
    # workspace manifests and every crate; drop everything else.
    filter =
      path: _type:
      let
        rel = lib.removePrefix "${toString src}/" (toString path);
      in
      rel == "Cargo.toml" || rel == "Cargo.lock" || rel == "crates" || lib.hasPrefix "crates/" rel;
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
  inherit version;
  src = cargoSrc;

  cargoLock = {
    lockFile = ../Cargo.lock;
    # smithay is pinned to upstream ff5fa7d (xkbcommon 0.9 → wvkbd keymap fix).
    # git deps must be vendored with an explicit hash.
    outputHashes = {
      "smithay-0.7.0" = "sha256-TV/GTfSvgfVwIFUGoASU7xm38opIBLjLMf1HeNTW07U=";
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
    # libdbus: the iio-sensor-proxy client (accelerometer orientation).
    dbus
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
    "-p"
    "sc-search"
  ];

  # Workspace tests need no display; keep them on so `nix flake check` is useful.
  cargoTestFlags = [ "--workspace" ];

  postInstall = ''
    # `$out/bin` on PATH so the compositor can spawn the sibling `sc-search`
    # binary (the pull-down search app) by name.
    wrapProgram $out/bin/springchick \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}" \
      --prefix PATH : "${lib.makeBinPath [ xwayland ]}:$out/bin"

    # The search app is an eframe/glow Wayland client: it needs the GL + wayland
    # libs at runtime the same way the compositor does.
    wrapProgram $out/bin/sc-search \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}"

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
