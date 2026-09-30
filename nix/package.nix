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
  # The sandbox forbids skia-bindings' download, so fetch the prebuilt archive
  # and hand it over as a file:// SKIA_BINARIES_URL.
  # Name: `skia-binaries-<repo hash>-<target>-<features>`. On a skia-safe bump,
  # update all three values from
  # https://github.com/rust-skia/skia-binaries/releases. `gl` resolves to
  # features `gl-jpegd-jpege-pdf`.
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

  # Only files cargo reads, so edits under nix/, tests/, docs/ don't rebuild.
  cargoSrc = lib.cleanSourceWith {
    inherit src;
    # lib.fileset rejects the string-like flake `self`; cleanSourceWith doesn't.
    filter =
      path: _type:
      let
        rel = lib.removePrefix "${toString src}/" (toString path);
      in
      rel == "Cargo.toml" || rel == "Cargo.lock" || rel == "crates" || lib.hasPrefix "crates/" rel;
  };

  # dlopen'd at runtime, so they go on LD_LIBRARY_PATH.
  # Not mesa: its libEGL would shadow glvnd and the host's driver (the stock
  # mesa has no panfrost for the Mali-G715, so llvmpipe).
  runtimeLibs = [
    wayland
    libxkbcommon
    libGL
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
    # Upstream smithay git (xkbcommon 0.9 fixes wvkbd keymaps).
    outputHashes = {
      "smithay-0.7.0" = "sha256-FkybYhnZ6h5EQIROWzNTGD7zk9fH3WwNzomWr3ebbzA=";
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
    # iio-sensor-proxy client.
    dbus
    fontconfig
    freetype
    expat
    zlib
  ];

  env.SKIA_BINARIES_URL = "file://${skiaBinaries}";

  cargoBuildFlags = [
    "-p"
    "sc-compositor"
    "-p"
    "sc-search"
  ];

  cargoTestFlags = [ "--workspace" ];

  postInstall = ''
    # `$out/bin` on PATH so the compositor can spawn `sc-search`.
    wrapProgram $out/bin/springchick \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}" \
      --prefix PATH : "${lib.makeBinPath [ xwayland ]}:$out/bin"

    wrapProgram $out/bin/sc-search \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}"

    # Starts springchick.service (Type=notify) instead of exec'ing the
    # compositor, so the service's BindsTo can raise graphical-session.target.
    install -Dm555 ${./springchick-session} $out/bin/springchick-session
    substituteInPlace $out/bin/springchick-session \
      --replace-fail '@springchick@' "$out/bin/springchick"

    install -Dm444 ${./springchick.desktop} \
      $out/share/wayland-sessions/springchick.desktop

    install -Dm444 ${../config.example.toml} \
      $out/share/springchick/config.example.toml
  '';

  # Must match the desktop file's DesktopNames.
  passthru.providedSessions = [ "springchick" ];

  meta = {
    description = "iOS Springboard-style Wayland compositor";
    mainProgram = "springchick";
    license = lib.licenses.gpl2Only;
    platforms = lib.platforms.linux;
  };
}
