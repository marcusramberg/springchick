# Task-switcher / quick-switcher MRU interaction test.
#
# Launches three foot windows with distinct background colours and app_ids
# (red/green/blue), then drives synthetic gestures through the shipped
# `springchick ipc` client (over the compositor's control socket) to exercise:
#   - quick-switch (horizontal bar swipe) BROWSES without reordering the MRU;
#   - picking a card in the task switcher promotes it to the front of the MRU.
# The distinct colours make the front-most app identifiable in screenshots; the
# distinct app_ids make the `state changed to ...` journal lines self-documenting.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-switcher -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # A foot launcher with a solid background colour and a matching app_id, plus a
  # .desktop file whose stem == app_id so the catalog resolves it (app_id fix).
  colorApp =
    name: hex:
    pkgs.makeDesktopItem {
      inherit name;
      desktopName = name;
      # Solid background fills the window, so a screenshot of the front app is a
      # near-uniform block of this colour. `sleep` keeps the client mapped.
      exec = "${pkgs.foot}/bin/foot --app-id=${name} -o colors-dark.background=${hex} -e sleep 6000";
    };

  redApp = colorApp "red" "cc0000";
  greenApp = colorApp "green" "00aa00";
  blueApp = colorApp "blue" "0000cc";
in
mkTest {
  name = "springchick-switcher";

  # springchick itself is on PATH via the module, so the test drives gestures
  # with the shipped `springchick ipc` client. The control socket uses its
  # default path ($XDG_RUNTIME_DIR/springchick-ipc.sock); the root test driver
  # reaches it by pointing the client there explicitly.
  packages = [
    pkgs.foot
    redApp
    greenApp
    blueApp
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def dbg(line):
        # Drive one gesture via the shipped client. SPRINGCHICK_IPC_SOCK points at
        # the tester's default runtime socket; `springchick` is on PATH via the
        # module. The client blocks for the compositor's reply and exits non-zero
        # on error, so machine.succeed doubles as an assertion the verb was taken.
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    # Toplevels are numbered in map order, so launching red, green, blue fixes a
    # stable identity per window. The toplevel id is what the assertions key on: it is
    # correct across every path (launch, switcher-tap, quick-switch) regardless
    # of app_id-resolution timing, and never races the zoom animation.
    TID = {"red": 0, "green": 1, "blue": 2}

    def launch(color, hexc):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit=app-{color} "
            f"--setenv=WAYLAND_DISPLAY={sock} $(command -v foot) "
            f"--app-id={color} -o colors-dark.background={hexc} -e sleep 6000"
        )
        wait_front(color)  # each app maps to the foreground before the next

    def wait_front(color):
        # Block until the NEWEST 'state changed to App { ... }' is this toplevel.
        # Deterministic where settle+grep raced the open/zoom animation.
        tid = TID[color]
        machine.wait_until_succeeds(
            f"{JOURNAL} "
            r"""| grep -oE 'state changed to App \{ toplevel: [0-9]+' """
            f"| tail -1 | grep -qE 'toplevel: {tid}$'",
            timeout=25,
        )

    # Launch order fixes the MRU: red, green, blue -> stack [blue, green, red],
    # front = blue. Distinct colours make the front app obvious in screenshots;
    # distinct app_ids make the journal lines self-describing.
    launch("red", "cc0000")
    launch("green", "00aa00")
    launch("blue", "0000cc")
    wait_front("blue")
    machine.screenshot("01-launched-blue-front")

    # Gesture coordinates are in physical output pixels. The output is the shared
    # phone profile, 720x1440 portrait; the switcher card geometry is proportional
    # to output size, so each coordinate below is a fixed fraction of that (noted
    # inline) and would rescale cleanly if the profile ever changed.

    # --- Task switcher: picking a card promotes it to the front of the MRU ---
    # Enter the switcher deck: a slow upward bar swipe into the middle band
    # (x=0.5W; y 0.985H -> 0.7125H, up_progress ~0.27, below the home threshold,
    # no flick velocity).
    dbg("swipe 360 1418 360 1026 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    machine.screenshot("02-switcher-deck")  # blue front, green mid, red back

    # Tap the GREEN card (its exposed strip; blue's front-card rect starts at
    # ~0.32W ~= x=232, so x=163 (0.227W) unambiguously hits green). Selecting it
    # calls push_foreground -> green to the front, MRU becomes [green, blue, red].
    dbg("tap 163 720")
    wait_front("green")
    machine.screenshot("03-after-tap-green")

    # --- Quick-switch: horizontal bar swipe BROWSES the MRU without reordering ---
    # Handedness matches the carousel (most-recent on the right): from the
    # front app, swiping RIGHT walks toward older apps. Green is now front, so
    # swipe-right walks the (unchanged) stack [green, blue, red]:
    # green -> blue -> red. (Swiping left would rubber-band: nothing is more
    # recent than the front app.)
    dbg("swipe 360 1418 608 1418 500")
    wait_front("blue")
    machine.screenshot("04-qs-blue")

    dbg("swipe 360 1418 608 1418 500")
    wait_front("red")
    machine.screenshot("05-qs-red")

    # Reaching red on the SECOND consecutive right-swipe is the proof that
    # quick-switch does not reorder: it walked green -> blue -> red down the MRU
    # stack the switcher tap set. Had the first swipe promoted blue to the front
    # (push_foreground), the second would have bounced back to green, never red.
    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
