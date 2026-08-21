# Rounded-card mask test: a card must show the client's pixels, with its
# corners rounded away — for a client whose buffer is NOT the size of the rect
# it is drawn at.
#
# The client is `weston-scaler -b`, which sets both a src crop and a dst size on
# a wp_viewport. That is the waydroid geometry (Android renders into a fixed
# gralloc buffer and viewport-maps it onto the window) and it is what the
# rounded-corner shader used to get wrong: the mask ran off `v_coords`, which is
# the quad coordinate already mapped through the buffer's `tex_matrix`, so a
# viewport crop made it collapse to a constant — 0 painted the card solid black
# (smithay disables blending over the opaque region, so zero is written, not
# skipped), 1 dropped the rounding.
#
# The two assertions are the two ways it broke, in the order they were found:
#   - the card is not a black (or empty) rectangle — content reaches the deck;
#   - the card's corner is not its own content — the mask still rounds.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-card-mask -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;
in
mkTest {
  name = "springchick-card-mask";

  packages = [ pkgs.weston ];
  # The assertions read pixels out of the screenshot.
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

    # -b: both src and dst set on the viewport, i.e. the buffer is cropped and
    # scaled onto the window rather than being the window's size.
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

    # Same entry gesture as the switcher test: a slow bar swipe into the middle
    # band, no flick velocity, short of the home threshold.
    dbg("swipe 360 1418 360 1026 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    dbg("settle 2000")
    machine.screenshot("02-switcher-card")

    # The front card slot, from the deck's own geometry (switcher.rs): a card is
    # FRONT_SCALE of the output, its centre one card-half plus a 0.06W margin in
    # from the right edge, vertically centred.
    W, H = ${toString phone.width}, ${toString phone.height}
    FRONT_SCALE = 0.62
    cw, ch = W * FRONT_SCALE, H * FRONT_SCALE
    cx, cy = W - cw / 2 - W * 0.06, H / 2
    left, top = int(cx - cw / 2), int(cy - ch / 2)
    right, bottom = int(cx + cw / 2), int(cy + ch / 2)

    # `machine.screenshot` writes into $out under `nix build`, and into the
    # working directory under the interactive driver.
    img = Image.open(
        os.path.join(os.environ.get("out", "."), "02-switcher-card.png")
    ).convert("RGB")

    # Pixels come out as bytes rather than via getdata(), which the driver's
    # type checker rejects: `tobytes()` is plain `bytes`, so a pixel is a
    # 3-byte slice and its components are ints.
    def pixels(box):
        raw = img.crop(box).tobytes()
        return [raw[i : i + 3] for i in range(0, len(raw), 3)]

    def differs(a, b):
        return abs(a[0] - b[0]) + abs(a[1] - b[1]) + abs(a[2] - b[2]) > 40

    def colours(box):
        return set(pixels(box))

    # A 60%-of-the-card centre patch: all card, whatever the client's own window
    # geometry turned out to be inside the slot.
    # The shell's own backdrop, sampled well clear of the deck. Everything below
    # is "is this pixel the client's, or the backdrop's".
    backdrop = pixels((8, 8, 24, 24))[0]

    # weston-scaler keeps its own window size rather than taking the maximize,
    # so its card fills only the top-left of the slot. Sample well inside that.
    centre = pixels((left + 40, top + 40, left + 120, top + 120))
    corner = pixels((left, top, left + 6, top + 6))

    # (1) Content reaches the card. The black-square bug filled the card with
    # (0,0,0); the empty-outline bug left only the shadow and the depth scrim,
    # i.e. the backdrop. Neither is the client's own pixels.
    content = sum(1 for p in centre if differs(p, backdrop)) / len(centre)
    black = sum(1 for p in centre if max(p) < 24) / len(centre)
    assert content > 0.8, f"only {content:.0%} of the card is client pixels — card is empty"
    assert black < 0.5, f"card centre is {black:.0%} black — the card is painting itself out"

    # (2) The corner is still rounded away. With the mask collapsed to 1 (or
    # sitting off the card) this square is client pixels like the centre.
    cut = sum(1 for p in corner if not differs(p, backdrop)) / len(corner)
    assert cut > 0.8, f"card corner is only {cut:.0%} backdrop — the corner mask is not rounding"

    # NOT covered here: the element-order half of the card bug — a slice is
    # front-to-back, so drawing it as `[..1]` then `[1..]` paints the topmost
    # surface underneath the root. Reproducing that needs a client whose root is
    # opaque and *fully covered* by a subsurface, which is what a waydroid card
    # is. `weston-subsurfaces` is not it: its subsurfaces are not covered by the
    # root, so it renders identically either way (verified — the buggy draw
    # passes an assertion built on it). A regression test wants a purpose-built
    # client; until then that half is only verified on-device.

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
