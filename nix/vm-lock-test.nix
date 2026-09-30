# ext-session-lock-v1 with swaylock. A lock hides the session and is
# confirmed only after a locked frame; swaylock's colour is what's on screen;
# gestures are dead while locked; typing the password unlocks; a lock client
# that dies leaves a black, still-locked screen.
#
# Keys go through `springchick ipc key`, the real xkb/forwarding path.
#
# Run:  nix build .#checks.aarch64-linux.vm-lock
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # Solid colour, so "is the screen this colour" is the oracle.
  lockColor = "00cc00";

in
mkTest {
  name = "springchick-lock";

  packages = [ pkgs.swaylock ];

  extraPythonPackages = p: [ p.pillow ];

  extraMachineConfig = {
    # Without a PAM service swaylock never locks.
    security.pam.services.swaylock = { };
    users.users.tester.password = "swordfish";
  };

  testScript = ''
    from PIL import Image

    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"
    W, H = 720, 1440

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def lock_client(unit):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit={unit} "
            f"--setenv=WAYLAND_DISPLAY={sock} "
            f"$(command -v swaylock) --color ${lockColor} --indicator-idle-visible"
        )

    def screen(name):
        """(mean_rgb, brightest_channel) of what is on screen. The mean is over
        a patch away from the centre, so swaylock's indicator ring can't skew
        it; the brightest channel anywhere separates a black screen from the
        (dark, but icon-covered) home screen."""
        machine.screenshot(name)
        img = Image.open(f"{machine.out_dir}/{name}.png").convert("RGB")
        w, h = img.size
        # Bytes, not PIL tuples, to keep the driver's type checker happy.
        patch = img.crop((int(w * 0.1), int(h * 0.1), int(w * 0.3), int(h * 0.3))).tobytes()
        n = len(patch) // 3
        mean = (
            sum(patch[0::3]) // n,
            sum(patch[1::3]) // n,
            sum(patch[2::3]) // n,
        )
        return mean, max(img.tobytes())

    def is_lock_green(rgb):
        r, g, b = rgb
        return g > 100 and r < 80 and b < 80

    def locked_count():
        return int(machine.succeed(f"{JOURNAL} | grep -c 'session locked' || true").strip())

    def unlocked_count():
        return int(machine.succeed(f"{JOURNAL} | grep -c 'session unlocked' || true").strip())

    lock_client("swaylock")

    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session lock requested'", timeout=60)
    # Only confirmed once a locked frame was presented.
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session locked'", timeout=60)

    rgb, _ = screen("01-locked")
    assert is_lock_green(rgb), (
        f"expected swaylock's green lock surface to cover the screen, sampled rgb={rgb}"
    )

    # A bar swipe up must move nothing behind the lock.
    before = machine.succeed(f"{JOURNAL} | grep -c 'state changed to' || true").strip()
    dbg(f"swipe {W // 2} {H - 5} {W // 2} {H // 3} 300")
    dbg("settle 500")
    dbg(f"tap {W // 2} {H // 2}")
    dbg("settle 500")
    after = machine.succeed(f"{JOURNAL} | grep -c 'state changed to' || true").strip()
    assert before == after, (
        f"the shell changed state behind the lock ({before} -> {after} transitions)"
    )
    rgb, _ = screen("02-still-locked")
    assert is_lock_green(rgb), f"gestures leaked past the lock, sampled rgb={rgb}"

    for ch in "swordfish":
        dbg(f"key {ch}")
    dbg("key Return")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session unlocked'", timeout=60)
    dbg("settle 1000")
    rgb, brightest = screen("03-unlocked")
    assert not is_lock_green(rgb), (
        f"the lock surface is still on screen after unlocking, sampled rgb={rgb}"
    )
    # Home has bright chrome (pill, dots, labels), so this isn't just black.
    assert brightest > 60, f"nothing was drawn after unlocking (brightest channel {brightest})"

    # Fail closed: a dying lock client stays locked.
    locks = locked_count()
    lock_client("swaylock2")
    machine.wait_until_succeeds(
        f"test $({JOURNAL} | grep -c 'session locked') -gt {locks}", timeout=60
    )
    machine.succeed("systemctl --user -M tester@.host kill -s KILL swaylock2")
    dbg("settle 1000")
    # Black, never the session.
    rgb, brightest = screen("04-lock-client-died")
    assert brightest < 30, (
        f"the session reappeared after the lock client died (brightest channel {brightest})"
    )
    assert unlocked_count() == 1, (
        f"the session unlocked itself when the lock client died ({unlocked_count()} unlocks)"
    )

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
