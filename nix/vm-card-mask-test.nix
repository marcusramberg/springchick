# Rounded card mask for a client whose buffer isn't its drawn size.
# `weston-scaler -b` sets a viewport crop and destination, like waydroid. The
# card must show the client's pixels (not black or empty) and still have its
# corners rounded away.
#
# Run:  nix build .#checks.aarch64-linux.vm-card-mask -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;
in
mkTest {
  name = "springchick-card-mask";

  packages = [ pkgs.weston ];
  extraPythonPackages = p: [ p.pillow ];

  testScript = ''
    import os

    from PIL import Image

    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    # -b: the buffer is cropped and scaled onto the window.
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=app-scaler "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v weston-scaler) -b"
    )
    machine.wait_until_succeeds(
        f"{JOURNAL} " r"""| grep -oE 'state changed to App \{ toplevel: [0-9]+' """
        "| tail -1 | grep -q .",
        timeout=30,
    )
    dbg("settle 2000")
    machine.screenshot("01-scaler-fullscreen")

    # Slow bar swipe into the middle band, as in vm-switcher.
    dbg("swipe 360 1418 360 1026 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    dbg("settle 2000")
    machine.screenshot("02-switcher-card")

    # Front slot, from switcher.rs: FRONT_SCALE of the output, 0.06W margin from
    # the right, vertically centred.
    W, H = ${toString phone.width}, ${toString phone.height}
    FRONT_SCALE = 0.62
    cw, ch = W * FRONT_SCALE, H * FRONT_SCALE
    cx, cy = W - cw / 2 - W * 0.06, H / 2
    left, top = int(cx - cw / 2), int(cy - ch / 2)
    right, bottom = int(cx + cw / 2), int(cy + ch / 2)

    # $out under `nix build`, cwd under the interactive driver.
    img = Image.open(
        os.path.join(os.environ.get("out", "."), "02-switcher-card.png")
    ).convert("RGB")

    # `tobytes()`, not getdata(), which the driver's type checker rejects.
    def pixels(box):
        raw = img.crop(box).tobytes()
        return [raw[i : i + 3] for i in range(0, len(raw), 3)]

    def differs(a, b):
        return abs(a[0] - b[0]) + abs(a[1] - b[1]) + abs(a[2] - b[2]) > 40

    def colours(box):
        return set(pixels(box))

    # Sampled well clear of the deck.
    backdrop = pixels((8, 8, 24, 24))[0]

    # weston-scaler keeps its own size, filling only the slot's top-left.
    centre = pixels((left + 40, top + 40, left + 120, top + 120))
    corner = pixels((left, top, left + 6, top + 6))

    # (1) Client pixels, not black and not just shadow/scrim over the backdrop.
    content = sum(1 for p in centre if differs(p, backdrop)) / len(centre)
    black = sum(1 for p in centre if max(p) < 24) / len(centre)
    assert content > 0.8, f"only {content:.0%} of the card is client pixels — card is empty"
    assert black < 0.5, f"card centre is {black:.0%} black — the card is painting itself out"

    # (2) The corner is rounded away.
    cut = sum(1 for p in corner if not differs(p, backdrop)) / len(corner)
    assert cut > 0.8, f"card corner is only {cut:.0%} backdrop — the corner mask is not rounding"

    # Not covered: the element-order bug (splitting the draw painted the top
    # surface under an opaque root). Needs a client whose root is fully covered
    # by a subsurface, like waydroid; weston-subsurfaces isn't. On-device only.

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
