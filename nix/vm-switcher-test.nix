# Switcher MRU: three foot windows (red/green/blue). Quick-switch browses
# without reordering; picking a card in the switcher moves it to the front.
#
# Run:  nix build .#checks.aarch64-linux.vm-switcher -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # .desktop stem == app_id so the catalog resolves it.
  colorApp =
    name: hex:
    pkgs.makeDesktopItem {
      inherit name;
      desktopName = name;
      # `sleep` keeps the client mapped.
      exec = "${pkgs.foot}/bin/foot --app-id=${name} -o colors-dark.background=${hex} -e sleep 6000";
    };

  redApp = colorApp "red" "cc0000";
  greenApp = colorApp "green" "00aa00";
  blueApp = colorApp "blue" "0000cc";
in
mkTest {
  name = "springchick-switcher";

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
        # Exits non-zero on an error reply, so succeed() asserts the verb was taken.
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    # Toplevel ids follow map order and are stable across every path.
    TID = {"red": 0, "green": 1, "blue": 2}

    def launch(color, hexc):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit=app-{color} "
            f"--setenv=WAYLAND_DISPLAY={sock} $(command -v foot) "
            f"--app-id={color} -o colors-dark.background={hexc} -e sleep 6000"
        )
        wait_front(color)  # each app maps to the foreground before the next

    def wait_front(color):
        # Wait for the newest `state changed to App` to be this toplevel.
        tid = TID[color]
        machine.wait_until_succeeds(
            f"{JOURNAL} "
            r"""| grep -oE 'state changed to App \{ toplevel: [0-9]+' """
            f"| tail -1 | grep -qE 'toplevel: {tid}$'",
            timeout=25,
        )

    # MRU becomes [blue, green, red].
    launch("red", "cc0000")
    launch("green", "00aa00")
    launch("blue", "0000cc")
    wait_front("blue")
    machine.screenshot("01-launched-blue-front")

    # Physical px on the 720x1440 phone profile.

    # Enter the switcher: slow bar swipe into the middle band (up_progress
    # ~0.27, below home, no flick).
    dbg("swipe 360 1418 360 1026 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    machine.screenshot("02-switcher-deck")  # blue front, green mid, red back

    # Green's exposed strip; blue's front card starts at ~x=232. MRU becomes
    # [green, blue, red].
    dbg("tap 163 720")
    wait_front("green")
    machine.screenshot("03-after-tap-green")

    # Quick-switch: swiping right walks toward older apps without reordering,
    # green -> blue -> red.
    dbg("swipe 360 1418 608 1418 500")
    wait_front("blue")
    machine.screenshot("04-qs-blue")

    dbg("swipe 360 1418 608 1418 500")
    wait_front("red")
    machine.screenshot("05-qs-red")

    # Reaching red on the second swipe proves no reorder: a promoted blue would
    # have bounced back to green.
    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
