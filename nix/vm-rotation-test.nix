# Landscape-rotation test: fullscreen turns the app a quarter turn.
#
# Policy under test (see crates/sc-compositor/src/rotation.rs): a toplevel that
# goes fullscreen is configured at the *swapped* size and drawn rotated, and
# goes back to portrait when it leaves fullscreen. springchick's own chrome
# stays portrait.
#
# The client is `imv` showing a four-quadrant image, one saturated colour per
# corner, at exactly the landscape aspect so it fills the rotated area with no
# letterboxing. That makes the screenshot self-describing: sampling the four
# screen quadrants says not just "something rotated" but *which way*. For a
# quarter turn clockwise the image's top-left corner ends up at the screen's
# top-right, so:
#
#     image            screen
#     R G      ->      B R
#     B Y              Y G
#
# A counter-clockwise turn would put them at the mirrored corners, so this
# catches a flipped `Rotation::transform()` — the failure mode that is invisible
# to a "did it rotate?" assertion.
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

    # --- Fullscreen -> landscape -----------------------------------------
    # imv -f asks for fullscreen at map time, so the compositor's fullscreen
    # path runs before the client has ever drawn portrait.
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=imv "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v imv) "
        "-f -i rotation-test ${quadrants}"
    )

    # The configure carries the SWAPPED logical size: the output is W x H
    # physical at dpi ${toString phone.dpi}, so landscape logical is H/dpi x W/dpi.
    want_w = int(H / ${toString phone.dpi})
    want_h = int(W / ${toString phone.dpi})
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'fullscreen request; configure {want_w}x{want_h} landscape'",
        timeout=30,
    )
    # Rotation only engages once the client has ACKED that configure and drawn
    # at the landscape size — never on the request alone.
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation Landscape'", timeout=30)

    # Let the client paint the full-size buffer before sampling.
    machine.sleep(3)
    machine.screenshot("02-landscape")

    got = quadrant_colours("02-landscape")
    # Clockwise quarter turn: image top-left lands at the screen's top-right.
    want = {
        "top-left": "blue",
        "top-right": "red",
        "bottom-left": "yellow",
        "bottom-right": "green",
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
    assert rotated_px == COLOURS["yellow"], (
        f"expected app content (yellow) at {KEYBOARD_PROBE} while rotated, "
        f"found {rotated_px} — the layer surface is still being drawn over the "
        "rotated app"
    )

    # --- Leaving fullscreen -> portrait ----------------------------------
    # Quitting the client is the same path as an app exiting fullscreen from the
    # compositor's side: the foreground toplevel stops being fullscreen.
    machine.succeed("systemctl --user -M tester@.host stop imv")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation None'", timeout=30)
    machine.screenshot("03-back-to-portrait")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
