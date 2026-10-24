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

  # AOSP LatinIME word lists with per-word frequencies, for sc-osk's
  # completion. googlesource serves raw files base64-encoded.
  aospRev = "127336e9f29d69607eab55982324b210279ae8c5";
  aospList = name: hash:
    fetchurl {
      url = "https://android.googlesource.com/platform/packages/inputmethods/LatinIME/+/${aospRev}/dictionaries/${name}_wordlist.combined.gz?format=TEXT";
      name = "${name}_wordlist.combined.gz.b64";
      inherit hash;
    };
  predictLists = lib.mapAttrs aospList {
    cs = "sha256-MU+0rWm0H+blXWpmnilY2j2VQDOGYsKyOy2D32Bopd8=";
    da = "sha256-xVk4ariFdPb0NhJpARUPKX68pYunpsJr+mUbSKdIH34=";
    de = "sha256-H33Q5SacURIL2UAxmpD9s791Ve34iszNAmmA+ewwVqA=";
    el = "sha256-0/0JirVQ0RauliBnRgFGGZH96jb4UAXuZtSSxVQ0rl4=";
    en_GB = "sha256-whDaZ64QeDaTDOHOKpS4D2QrS1AjvaOJ0rOBMEZv8Wg=";
    en_US = "sha256-nrQdQh3sqR7XmrIr4pXQwqvlMohgOkmUxIja5l1ZMsw=";
    en = "sha256-fRVlQ8+n2+eabRHi62XG1Jt68qyytee1q84dUL69hE4=";
    es = "sha256-mBv2R4gGERQlC8jb4nvJFdeemhTTe9CR74Xe6NfWG24=";
    fi = "sha256-AgJgaPpJyul0KHD+9DF3BSdnMuLaqd/pI4oewStw1RA=";
    fr = "sha256-xcRtxYjOjD6pa2p7W8MTVLpIgzJ8dQVhztfNbPXaQao=";
    hr = "sha256-UraCvZm82gRRvqHy4wssLIOUMS5wYdDRezzfDRAFMCU=";
    it = "sha256-5XjtixsgISqpthfYGXzHNQN/00+X+giK7RYEmogaSgY=";
    iw = "sha256-vlZtxg24m14R5/1ovbiPhwyo6ycwYJd9e3C87KQnT1g=";
    lt = "sha256-f9NUH4Ut30PkRL8ilJTz70rGn4GHtKM7nzVpcpWEfF0=";
    lv = "sha256-kJyW0tOEabfbttZSukskNtLOTrsn2vl0qaqlL5YpAsQ=";
    nb = "sha256-IMyr0ruhA975w09FQxVbeIPPuGIuUiM3dRxe/l8WotA=";
    nl = "sha256-UPMuUw7GQhH2lgxRYFKWVm0TwuDj/ReeoOuo1lyRN2Q=";
    pl = "sha256-c0UXzw3L6QN+c6GipcOqOy0S2YFA9b6JNZloamRafTw=";
    pt_BR = "sha256-tbwZx/4LGEZ3CS3zJjLjzGZTrFrp9Rmhw7cjQdJFg4U=";
    pt_PT = "sha256-FGvjx50hInu2wUT7EFnGXXHSh/1G8RRGT/cL6WTMcJs=";
    ro = "sha256-fIczPe5uhVtWG5c3fRrESdeVWcLR+YidDEHsJPYL1d0=";
    ru = "sha256-EGoYKwPW4YLNF6h69frFkDt/pLY0se/Tei9+QFgyQdA=";
    sl = "sha256-7gyGxqGyMFFmlRvI+Rq0zkojb/TUhU0zAO0R5XFZN+0=";
    sr = "sha256-BimRUr5UdIb8jPvckOPtNRkHI+d/gvi1WhfIJXq631A=";
    sv = "sha256-7d9RXP8jXKpqnWQDl95nO8S8sTYgv+Xmwe1IiLLZITU=";
    tr = "sha256-pLY49Vxw5e8f4MFFDasOwWq1VAVlPnd0+9LyREQd5Wg=";
  };

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
      "smithay-0.7.0" = "sha256-s5PFS0TQd+5ueI9AKRFloa3e5jir1GtwH+dckZ6e7yA=";
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
    "-p"
    "sc-osk"
  ];

  cargoTestFlags = [ "--workspace" ];

  postInstall = ''
    # `$out/bin` on PATH so the compositor can spawn `sc-search`.
    wrapProgram $out/bin/springchick \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}" \
      --prefix PATH : "${lib.makeBinPath [ xwayland ]}:$out/bin"

    wrapProgram $out/bin/sc-search \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}"

    wrapProgram $out/bin/sc-osk \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}" \
      --prefix XDG_DATA_DIRS : "$out/share"

    mkdir -p $out/share/springchick/predict
    ${lib.concatStringsSep "\n" (lib.mapAttrsToList (name: b64: ''
      base64 -d ${b64} > ${name}.combined.gz
      python3 ${../scripts/aosp-wordlist.py} ${name}.combined.gz \
        > $out/share/springchick/predict/${name}.txt
    '') predictLists)}

    # Starts springchick.service (Type=notify) instead of exec'ing the
    # compositor, so the service's BindsTo can raise graphical-session.target.
    install -Dm555 ${./springchick-session} $out/bin/springchick-session
    substituteInPlace $out/bin/springchick-session \
      --replace-fail '@springchick@' "$out/bin/springchick"

    install -Dm444 ${./springchick.desktop} \
      $out/share/wayland-sessions/springchick.desktop

    install -Dm444 ${../config.example.toml} \
      $out/share/springchick/config.example.toml

    # Inter is built into the binaries; the OFL travels with it.
    install -Dm444 ${../crates/sc-fonts/Inter-LICENSE.txt} \
      $out/share/licenses/springchick/Inter-LICENSE.txt
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
