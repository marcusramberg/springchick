# Landscape-rotation test: turning the device turns a fullscreen app.
#
# Policy under test (see crates/sc-compositor/src/rotation.rs): a fullscreen
# toplevel is configured at the size its orientation implies and drawn turned to
# match how the device is held; it goes back to portrait when the device is
# stood up again *or* when it stops being fullscreen. springchick's own chrome
# always stays portrait.
#
# Both halves of that rule are asserted, because the interesting bug was the
# first one: rotation used to key off fullscreen alone, so a fullscreen
# *portrait* app (the pull-down search) was configured at the swapped size and
# drawn with a rotated ghost of itself. Fullscreen-while-upright must therefore
# stay portrait, and only turning the device may rotate it.
#
# The VM has no accelerometer, so orientation arrives through the `orientation`
# control-socket verb — the same `set_device_orientation` entry point the
# iio-sensor-proxy client will feed.
#
# The client is `imv` showing a four-quadrant image, one saturated colour per
# corner, at exactly the landscape aspect so it fills the rotated area with no
# letterboxing. That makes the screenshot self-describing: sampling the four
# screen quadrants says not just "something rotated" but *which way*.
#
# Turning the phone CLOCKWISE puts its left edge up (`left-up`) and turns the
# app ANTICLOCKWISE, so the image's top-left corner ends up at the screen's
# bottom-left:
#
#     image          screen (left-up)      screen (right-up)
#     R G      ->        G Y                    B R
#     B Y                R B                    Y G
#
# The two are 180° apart, so asserting both catches the failure that a "did it
# rotate?" check cannot see: turning the app the same way as the phone, which
# renders video upside down. That is not hypothetical — it shipped, and was
# caught on device rather than here.
#
# It also covers the chrome rule: springchick's portrait chrome is suppressed
# while the app is rotated. wvkbd stands in for it as a real layer surface — the
# test proves it covers the bottom of the screen first, then that the same strip
# is app content once the app is fullscreen and rotated.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-rotation -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # Colours, as (name, hex, rgb) — the driver classifies sampled pixels against
  # these, so they are deliberately far apart in RGB space.
  colours = {
    red = "#cc0000";
    green = "#00aa00";
    blue = "#0000cc";
    yellow = "#cccc00";
  };

  # The rotated app area is the output with its axes swapped, so an image of
  # exactly that size fills it 1:1 under imv's default "full" scaling.
  imgW = phone.height;
  imgH = phone.width;
  halfW = imgW / 2;
  halfH = imgH / 2;

  quadrants =
    pkgs.runCommand "rotation-quadrants.png" { nativeBuildInputs = [ pkgs.imagemagick ]; } ''
      magick -size ${toString imgW}x${toString imgH} xc:black \
        -fill '${colours.red}'    -draw 'rectangle 0,0 ${toString (halfW - 1)},${toString (halfH - 1)}' \
        -fill '${colours.green}'  -draw 'rectangle ${toString halfW},0 ${toString (imgW - 1)},${toString (halfH - 1)}' \
        -fill '${colours.blue}'   -draw 'rectangle 0,${toString halfH} ${toString (halfW - 1)},${toString (imgH - 1)}' \
        -fill '${colours.yellow}' -draw 'rectangle ${toString halfW},${toString halfH} ${toString (imgW - 1)},${toString (imgH - 1)}' \
        $out
    '';
in
mkTest {
  name = "springchick-rotation";

  packages = [
    pkgs.imv
    # A real top/overlay layer surface, to prove layer chrome is hidden while
    # the app is rotated.
    pkgs.wvkbd
  ];
  # Pillow: the assertion is about pixels, not just that a screenshot exists.
  extraPythonPackages = p: [ p.pillow ];

  testScript = ''
    from PIL import Image

    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    def turn(orientation):
        """Report a device orientation, exactly as the accelerometer would.

        The VM has no accelerometer, so the `orientation` control verb stands in
        for iio-sensor-proxy. It feeds the same `State::set_device_orientation`
        the sensor will, so the policy under test is the real one.
        """
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc orientation {orientation}"
        ).strip()

    W, H = ${toString phone.width}, ${toString phone.height}
    COLOURS = {
        "red": (0xCC, 0x00, 0x00),
        "green": (0x00, 0xAA, 0x00),
        "blue": (0x00, 0x00, 0xCC),
        "yellow": (0xCC, 0xCC, 0x00),
    }

    def pixel(shot, x, y):
        """One RGB pixel from a saved screenshot."""
        image = Image.open(f"{machine.out_dir}/{shot}.png").convert("RGB")
        assert image.size == (W, H), f"screenshot is {image.size}, expected {(W, H)}"
        data = image.tobytes()
        i = (y * W + x) * 3
        return (data[i], data[i + 1], data[i + 2])

    def quadrant_colours(shot):
        """Classify the four screen quadrant centres by nearest reference colour.

        Sampling centres (not edges) keeps the result immune to the scaling
        blend along quadrant boundaries and to any rounding at the screen edge.
        """
        def at(x, y):
            px = pixel(shot, x, y)
            best = min(
                COLOURS,
                key=lambda name: sum(
                    (px[c] - COLOURS[name][c]) ** 2 for c in range(3)
                ),
            )
            return best, px

        return {
            "top-left": at(W // 4, H // 4),
            "top-right": at(3 * W // 4, H // 4),
            "bottom-left": at(W // 4, 3 * H // 4),
            "bottom-right": at(3 * W // 4, 3 * H // 4),
        }

    # --- Layer chrome is visible to begin with ----------------------------
    # wvkbd maps an overlay layer surface across the bottom of the screen. The
    # probe sits left of centre in that strip, clear of the home-bar pill (drawn
    # centred, and never hidden — it is the way back out of a fullscreen app).
    #
    # Comparing Home-before against keyboard-after proves the layer surface is
    # really on screen at the probe. Without that, the "hidden while rotated"
    # assertion below would pass just as happily with no keyboard at all.
    KEYBOARD_PROBE = (W // 4, H - 80)
    machine.screenshot("00-home")
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=wvkbd "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v wvkbd-mobintl)"
    )
    machine.sleep(3)
    machine.screenshot("01-keyboard-up")
    home_px = pixel("00-home", *KEYBOARD_PROBE)
    kbd_px = pixel("01-keyboard-up", *KEYBOARD_PROBE)
    assert kbd_px != home_px, (
        f"pixel at {KEYBOARD_PROBE} is unchanged ({home_px}) after wvkbd mapped "
        "— the layer surface never reached the screen, so the hiding assertion "
        "below would prove nothing"
    )

    # --- Fullscreen on an UPRIGHT phone stays portrait --------------------
    # The regression this policy exists for. springchick used to treat any
    # fullscreen toplevel as video wanting landscape, which configured portrait
    # apps at the swapped size and drew a rotated ghost of them. imv -f asks for
    # fullscreen at map time, so the fullscreen path runs immediately.
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=imv "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v imv) "
        "-f -i rotation-test ${quadrants}"
    )

    portrait_w = int(W / ${toString phone.dpi})
    portrait_h = int(H / ${toString phone.dpi})
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'fullscreen request; configure {portrait_w}x{portrait_h} None'",
        timeout=30,
    )
    machine.fail(f"{JOURNAL} | grep -qE 'rotation (LeftUp|RightUp)'")
    machine.sleep(2)
    machine.screenshot("02-fullscreen-upright")

    # Quadrant sampling is useless here: the test image is landscape-aspect on
    # purpose (it fills the *rotated* area exactly), so shown unrotated in a
    # portrait window it is letterboxed to a band across the middle and the
    # screen quadrant centres land on background, not image. Probe inside the
    # band instead — scaled to fit the width, it is W wide and W/2 tall,
    # centred vertically.
    band_top = (H - W // 2) // 2
    upright_probes = {
        # (x, y) -> the image quadrant it must land in.
        (W // 4, band_top + W // 8): "red",  # image top-left
        (3 * W // 4, band_top + 3 * W // 8): "yellow",  # image bottom-right
    }
    for (px, py), expected in upright_probes.items():
        got_px = pixel("02-fullscreen-upright", px, py)
        assert got_px == COLOURS[expected], (
            f"at ({px}, {py}) expected the unrotated image's {expected} "
            f"{COLOURS[expected]}, found {got_px}. A fullscreen app on an "
            "upright phone must not be rotated."
        )

    # --- Turning the device rotates it ------------------------------------
    turn("left-up")
    # The configure now carries the SWAPPED logical size: the output is W x H
    # physical at dpi ${toString phone.dpi}, so landscape logical is H/dpi x W/dpi.
    want_w = int(H / ${toString phone.dpi})
    want_h = int(W / ${toString phone.dpi})
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'fullscreen request; configure {want_w}x{want_h} LeftUp'",
        timeout=30,
    )
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation LeftUp'", timeout=30)

    # Let the client paint the full-size buffer before sampling.
    machine.sleep(3)
    machine.screenshot("02-landscape")

    got = quadrant_colours("02-landscape")
    # left-up = the phone turned CLOCKWISE, which turns the app ANTICLOCKWISE:
    # the image's top-left corner lands at the screen's bottom-left. Turning it
    # the same way as the phone would put the image 180° out — how this looked
    # on device before the transforms were swapped.
    want = {
        "top-left": "green",
        "top-right": "yellow",
        "bottom-left": "red",
        "bottom-right": "blue",
    }
    for corner, expected in want.items():
        name, px = got[corner]
        assert name == expected, (
            f"screen {corner} is {name} {px}, expected {expected}. "
            f"All corners: { {k: v[0] for k, v in got.items()} }. "
            "Mirrored corners mean the rotation turns the wrong way "
            "(Rotation::transform), not that rotation failed."
        )

    # Portrait chrome is suppressed while rotated: the strip that was keyboard a
    # moment ago is now the app's own bottom-left quadrant colour.
    rotated_px = pixel("02-landscape", *KEYBOARD_PROBE)
    assert rotated_px == COLOURS["red"], (
        f"expected app content (red) at {KEYBOARD_PROBE} while rotated, "
        f"found {rotated_px} — the layer surface is still being drawn over the "
        "rotated app"
    )

    # --- The other way up is the opposite turn ----------------------------
    # Guards the second landscape direction: right-up must be a 180° turn of
    # left-up, not the same transform (which would show it upside down).
    turn("right-up")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation RightUp'", timeout=30)
    machine.sleep(3)
    machine.screenshot("04-right-up")
    got = quadrant_colours("04-right-up")
    for corner, expected in {
        "top-left": "blue",
        "top-right": "red",
        "bottom-left": "yellow",
        "bottom-right": "green",
    }.items():
        name, px = got[corner]
        assert name == expected, (
            f"screen {corner} is {name} {px}, expected {expected} for right-up. "
            f"All corners: { {k: v[0] for k, v in got.items()} }. "
            "right-up should be left-up turned 180°."
        )

    # --- Standing the phone up again -> portrait --------------------------
    turn("normal")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation None'", timeout=30)
    machine.screenshot("05-upright-again")

    # --- Leaving fullscreen while turned -> portrait ----------------------
    # Quitting the client is the same path as an app exiting fullscreen from the
    # compositor's side: the foreground toplevel stops being fullscreen. Turn the
    # device first, so this proves fullscreen (not orientation) ended it.
    turn("left-up")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation LeftUp'", timeout=30)
    machine.succeed("systemctl --user -M tester@.host stop imv")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation None'", timeout=30)
    machine.screenshot("03-back-to-portrait")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
