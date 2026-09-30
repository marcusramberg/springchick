# Rotation: a fullscreen app turns with the device and returns to portrait
# when the device is upright again or it leaves fullscreen. A fullscreen app
# on an upright phone stays portrait. Layer chrome is hidden while turned.
#
# No accelerometer in the VM; the `orientation` ipc verb feeds the same
# `set_device_orientation` path.
#
# imv shows a four-colour image at exactly the landscape aspect, so sampling
# the screen quadrants tells which way it turned. Turning the phone
# clockwise (`left-up`) turns the app anticlockwise:
#
#     image          screen (left-up)      screen (right-up)
#     R G      ->        G Y                    B R
#     B Y                R B                    Y G
#
# The two are 180° apart, which catches turning the app the same way as the
# phone (video upside down; this shipped once).
#
# Run:  nix build .#checks.aarch64-linux.vm-rotation -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # Far apart in RGB so sampled pixels classify cleanly.
  colours = {
    red = "#cc0000";
    green = "#00aa00";
    blue = "#0000cc";
    yellow = "#cccc00";
  };

  # The rotated area is the output with axes swapped; this fills it 1:1.
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
    pkgs.foot
    # A real overlay layer surface, to show layers hide while turned.
    pkgs.wvkbd
  ];
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

        The turn is not instant: the reading is debounced (`rotation_settle_ms`,
        400ms) and the swap happens inside a dip to black (`rotation_fade_ms`).
        The sleeps at each call site cover both, and a screenshot taken during
        the dip would be uniformly black rather than merely wrong.
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

    # Layer chrome is visible to begin with. The probe is left of the home pill,
    # which is never hidden. Comparing against Home proves the keyboard is really
    # there, or the "hidden while rotated" check would pass with none.
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

    # Fullscreen on an upright phone stays portrait. imv -f is fullscreen at map.
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

    # The landscape image is letterboxed into a band (W wide, W/2 tall) in
    # portrait, so probe inside the band, not the screen quadrants.
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

    # Turning the device rotates it.
    turn("left-up")
    # Swapped logical size: H/dpi x W/dpi.
    want_w = int(H / ${toString phone.dpi})
    want_h = int(W / ${toString phone.dpi})
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'fullscreen request; configure {want_w}x{want_h} LeftUp'",
        timeout=30,
    )
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation LeftUp'", timeout=30)

    # Let the client paint the full-size buffer.
    machine.sleep(3)
    machine.screenshot("02-landscape")

    got = quadrant_colours("02-landscape")
    # Top-left of the image lands at the screen's bottom-left.
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

    # The strip that was keyboard is now app content.
    rotated_px = pixel("02-landscape", *KEYBOARD_PROBE)
    assert rotated_px == COLOURS["red"], (
        f"expected app content (red) at {KEYBOARD_PROBE} while rotated, "
        f"found {rotated_px} — the layer surface is still being drawn over the "
        "rotated app"
    )

    # right-up must be 180° from left-up, not the same transform.
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

    # Upright again -> portrait.
    turn("normal")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation None'", timeout=30)
    machine.screenshot("05-upright-again")

    # cmd-tab between two fullscreen apps stays landscape: the view holds its
    # turn through the switcher.
    def ipc(verb):
        return machine.succeed(f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {verb}").strip()

    def count(pattern):
        return int(machine.succeed(f"{JOURNAL} | grep -cF '{pattern}' || true").strip())

    turn("left-up")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'rotation LeftUp'", timeout=30)
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=imv2 "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v imv) "
        "-f -i rotation-test-2 ${quadrants}"
    )
    machine.sleep(4)
    machine.screenshot("06-second-app")
    before = count("rotation None")
    ipc("keydown Super_L")
    ipc("action switcher-next")
    ipc("settle 2000")
    machine.screenshot("07-deck-landscape")
    assert count("rotation None") == before, "the deck dropped to portrait"
    ipc("keyup Super_L")
    ipc("settle 2000")
    machine.sleep(2)
    machine.screenshot("08-switched-landscape")
    assert count("rotation None") == before, "switching fullscreen apps dropped to portrait"
    got = quadrant_colours("08-switched-landscape")
    assert got["bottom-left"][0] == "red", f"not landscape after switch: {got}"

    # Landing on a non-fullscreen app turns back.
    machine.succeed("systemctl --user -M tester@.host stop imv2")
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --unit=foot "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v foot)"
    )
    machine.wait_until_succeeds(f"[ $({JOURNAL} | grep -cF 'rotation None') -gt {before} ]", timeout=30)
    machine.sleep(2)
    machine.screenshot("09-portrait-app")
    machine.succeed("systemctl --user -M tester@.host stop foot")
    ipc("action home")

    # Leaving fullscreen while turned -> portrait. Turn first, so fullscreen (not
    # orientation) is what ends it.
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
