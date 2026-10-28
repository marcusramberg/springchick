# An app quick-switched away from with the OSK up must grow back once the
# OSK goes, or its switcher card stays short by the keyboard's height.
#
# Run:  nix build .#checks.aarch64-linux.vm-osk-card -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-osk-card";

  packages = [ pkgs.foot ];

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
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def run(unit, cmd):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit={unit} "
            f"--setenv=WAYLAND_DISPLAY={sock} {cmd}"
        )

    def wait_front(tid):
        machine.wait_until_succeeds(
            f"{JOURNAL} "
            r"""| grep -oE 'state changed to App \{ toplevel: [0-9]+' """
            f"| tail -1 | grep -qE 'toplevel: {tid}$'",
            timeout=25,
        )

    # Not launched by springchick, so the log names them by toplevel id.
    def h(tid):
        return (
            f"$({JOURNAL} | grep -oE 'toplevel size app_id=unknown_{tid} geometry=[0-9]+x[0-9]+' "
            "| tail -1 | grep -oE '[0-9]+$')"
        )

    def layers_have_zone():
        return f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc layers | grep -q excl=[1-9]"

    for tid, app in enumerate(["a", "b"]):
        run(f"app-{app}", f"$(command -v foot) --app-id={app} -e sleep 6000")
        wait_front(tid)
    dbg("settle 2000")
    # What b was configured to; foot's first commit can be a few px off it.
    full = int(
        machine.wait_until_succeeds(
            f"{JOURNAL} | grep -oE 'app_id=unknown_1 .* available=[0-9]+x[0-9]+' "
            "| tail -1 | grep -oE '[0-9]+$'",
            timeout=25,
        )
    )

    run("osk", "$(command -v sc-osk) --show")
    machine.wait_until_succeeds(layers_have_zone(), timeout=30)
    dbg("settle 2000")
    machine.wait_until_succeeds(f"[ {h(1)} -lt {full} ]", timeout=30)

    # Quick-switch right to the older app: b leaves the front shrunk.
    dbg("swipe 360 1418 608 1418 500")
    wait_front(0)
    dbg("settle 2000")

    machine.succeed("systemctl --user -M tester@.host stop osk")
    machine.wait_until_succeeds(f"! {layers_have_zone()}", timeout=30)
    dbg("settle 2000")
    machine.wait_until_succeeds(f"[ {h(0)} -eq {full} ]", timeout=30)
    machine.wait_until_succeeds(f"[ {h(1)} -eq {full} ]", timeout=30)

    dbg("swipe 360 1418 360 1026 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    dbg("settle 2000")
    machine.screenshot("switcher")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
