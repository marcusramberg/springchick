# ext-session-lock-v1 test with a real lock client (swaylock).
#
# The protocol's guarantee is a security one, so the oracles are about what is
# *not* on screen and what input can *not* do:
#   - a lock request hides the session and is confirmed to the client
#     (`session locked` is only logged after a locked frame was presented);
#   - the lock surface is what is actually composited — the screen shows
#     swaylock's colour, not the home screen behind it;
#   - the home gesture is dead while locked: the shell never changes state
#     behind the lock;
#   - typing the password unlocks, and the shell comes back;
#   - killing the lock client while locked leaves the session locked (a black
#     screen), never unlocked — the failure mode has to fail closed.
#
# swaylock is driven through the compositor's own IPC (`springchick ipc key`),
# which injects keys along the real xkb/filter/forward path, so the password
# reaches it exactly as a physical keyboard's would.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-lock
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # A solid, unmistakable colour: the checks below are "is the screen this
  # colour", which only works because swaylock paints the whole surface.
  lockColor = "00cc00";

in
mkTest {
  name = "springchick-lock";

  packages = [ pkgs.swaylock ];

  # The oracle reads pixels off the VM's framebuffer — the lock screen has
  # nothing legible on it by design, so OCR would have nothing to say.
  extraPythonPackages = p: [ p.pillow ];

  extraMachineConfig = {
    # swaylock authenticates through PAM; without this service it exits with
    # "failed to initialize pam" and never locks.
    security.pam.services.swaylock = { };
    # The password the test types back in to unlock.
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
        # Flat RGB bytes rather than PIL's pixel tuples: plain ints keep the
        # test driver's type checker happy and the arithmetic obvious.
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

    # --- Lock ---
    lock_client("swaylock")

    # The request arrived...
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session lock requested'", timeout=60)
    # ...and was confirmed, which the compositor only does once a frame drawn
    # under the lock has actually been presented.
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session locked'", timeout=60)

    # --- The lock surface is what is on screen ---
    rgb, _ = screen("01-locked")
    assert is_lock_green(rgb), (
        f"expected swaylock's green lock surface to cover the screen, sampled rgb={rgb}"
    )

    # --- Input can't reach the shell behind it ---
    # A swipe up from the home bar is the gesture that would otherwise take the
    # shell home / into the switcher; locked, it must move nothing.
    before = machine.succeed(f"{JOURNAL} | grep -c 'state changed to' || true").strip()
    dbg(f"swipe {W // 2} {H - 5} {W // 2} {H // 3} 300")
    dbg("settle 500")
    dbg(f"tap {W // 2} {H // 2}")
    dbg("settle 500")
    after = machine.succeed(f"{JOURNAL} | grep -c 'state changed to' || true").strip()
    assert before == after, (
        f"the shell changed state behind the lock ({before} -> {after} transitions)"
    )
    # And the screen is still the lock surface, not the home screen.
    rgb, _ = screen("02-still-locked")
    assert is_lock_green(rgb), f"gestures leaked past the lock, sampled rgb={rgb}"

    # --- Unlock by typing the password ---
    for ch in "swordfish":
        dbg(f"key {ch}")
    dbg("key Return")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'session unlocked'", timeout=60)
    dbg("settle 1000")
    rgb, brightest = screen("03-unlocked")
    assert not is_lock_green(rgb), (
        f"the lock surface is still on screen after unlocking, sampled rgb={rgb}"
    )
    # The shell really is back, not just a blank screen: the home screen has
    # bright chrome on it (the home pill, page dots, icon labels).
    assert brightest > 60, f"nothing was drawn after unlocking (brightest channel {brightest})"

    # --- Fail closed: a lock client that dies stays locked ---
    locks = locked_count()
    lock_client("swaylock2")
    machine.wait_until_succeeds(
        f"test $({JOURNAL} | grep -c 'session locked') -gt {locks}", timeout=60
    )
    machine.succeed("systemctl --user -M tester@.host kill -s KILL swaylock2")
    dbg("settle 1000")
    # No lock surface left, so the compositor draws black — and above all NOT
    # the session it was hiding.
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
