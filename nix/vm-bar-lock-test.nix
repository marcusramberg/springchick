# sc-bar's lock screen. A dark screen locks (`lock_on_blank`, through logind),
# a swipe up shows the pad, a wrong PIN stays locked, the right one unlocks,
# and a bar killed while locked comes back with the lock in hand.
#
# Run:  nix build .#checks.aarch64-linux.vm-bar-lock -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-bar-lock";

  extraMachineConfig =
    { lib, ... }:
    {
      programs.springchick.bar.enable = lib.mkForce true;
      programs.springchick.settings.main.lock_on_blank = true;
      users.users.tester.password = "2580";
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)
    machine.wait_until_succeeds("systemctl --user -M tester@.host is-active polkit-soteria.service", timeout=30)
    machine.wait_until_succeeds(
        f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc layers | grep -q 'ns=bar '", timeout=30
    )

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def count(what):
        return int(machine.succeed(f"{JOURNAL} | grep -c '{what}' || true").strip())

    def wait_count(what, n):
        machine.wait_until_succeeds(f"test $({JOURNAL} | grep -c '{what}') -ge {n}", timeout=60)

    # The pad at 720x1440, dpi 2 (`lock::key_center`).
    KEYS = {"2": (360, 544), "5": (360, 716), "8": (360, 888), "0": (360, 1059), "ok": (539, 1059)}

    def enter(pin):
        dbg("tswipe 360 1400 360 500 200")
        machine.sleep(2)
        for key in [*pin, "ok"]:
            dbg("tap %d %d" % KEYS[key])
            machine.sleep(1)

    # The screen goes dark, and the session locks behind it.
    dbg("action toggle-display")
    wait_count("session locked", 1)
    dbg("action toggle-display")
    machine.sleep(2)
    machine.screenshot("lock")

    dbg("tswipe 360 1400 360 500 200")
    machine.sleep(2)
    machine.screenshot("pad")
    dbg("tap %d %d" % KEYS["2"])
    dbg("tap %d %d" % KEYS["ok"])
    # Past pam_unix's delay on a wrong one.
    machine.sleep(6)
    assert count("session unlocked") == 0, "a wrong PIN unlocked the session"
    # Cancel: back to the cover.
    dbg("tap 180 1059")
    machine.sleep(2)

    enter("2580")
    wait_count("session unlocked", 1)

    # Killed under its own lock, the bar comes back and takes it up again.
    machine.succeed("loginctl lock-sessions")
    wait_count("session locked", 2)
    machine.succeed("systemctl --user -M tester@.host kill -s KILL sc-bar.service")
    wait_count("session locked", 3)
    machine.sleep(2)
    assert count("session unlocked") == 1, "the session unlocked when the bar died"
    enter("2580")
    wait_count("session unlocked", 2)

    # `loginctl unlock-session` is the way in for a password the pad can't type.
    machine.succeed("loginctl lock-sessions")
    wait_count("session locked", 4)
    machine.succeed("loginctl unlock-sessions")
    wait_count("session unlocked", 3)

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
