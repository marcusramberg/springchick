# Pointer support: a USB mouse driven through the QEMU monitor. Checks button
# routing, the cursor overlay appearing, and a touch hiding it.
#
# HMP `mouse_move` only injects relative motion (what the J09 ring emits), so
# the absolute-motion path is only covered by real hardware.
#
# Run:  nix build .#checks.aarch64-linux.vm-pointer -L
{ self, pkgs }:
let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-pointer";

  extraPythonPackages = p: [ p.pillow ];

  extraMachineConfig = {
    virtualisation.qemu.options = [
      "-device qemu-xhci,id=xhci"
      "-device usb-mouse,bus=xhci.0"
    ];
  };

  testScript = ''
    import time
    from PIL import Image

    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"


    def journal(pattern, timeout=15):
        machine.wait_until_succeeds(f"{JOURNAL} | grep -E '{pattern}'", timeout=timeout)


    def ipc(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()


    def light_px(name):
        """Near-white pixels on screen. The cursor is a white arrow outlined in
        black, and the home screen of an app-less VM is dark, so its arrival and
        departure are a step change in this count — which is what can be asserted
        without knowing where libinput's pointer acceleration parked it."""
        machine.screenshot(name)
        buf = Image.open(f"{machine.out_dir}/{name}.png").convert("RGB").tobytes()
        return sum(
            1
            for i in range(0, len(buf), 3)
            if buf[i] > 230 and buf[i + 1] > 230 and buf[i + 2] > 230
        )


    # The driver's own absolute tablet is active by default; select our relative
    # mouse or the motion goes elsewhere.
    mice = machine.send_monitor_command("info mice")
    print("QEMU mice:\n" + mice)
    mouse = None
    for line in mice.splitlines():
        # e.g. "* Mouse #2: QEMU HID Mouse"
        if "#" not in line or "absolute" in line:
            continue
        mouse = line.split("#", 1)[1].split(":", 1)[0].strip()
    assert mouse is not None, f"no relative pointer in: {mice}"
    machine.send_monitor_command(f"mouse_set {mouse}")

    ipc("settle 500")
    baseline = light_px("01-no-cursor")
    print(f"light px with no cursor: {baseline}")

    with subtest("relative motion reaches the compositor"):
        # Pin it in the top-left corner: with libinput acceleration a corner is the
        # only position a delta can guarantee.
        for _ in range(10):
            machine.send_monitor_command("mouse_move -200 -200")
            time.sleep(0.05)
        journal("pointer motion: dx=")

    with subtest("the cursor overlay is drawn"):
        ipc("settle 400")
        with_cursor = light_px("02-cursor")
        print(f"light px with cursor: {with_cursor}")
        assert (
            with_cursor > baseline + 60
        ), f"no cursor drawn: {baseline} -> {with_cursor} light px"

    with subtest("buttons reach the compositor as BTN_LEFT"):
        machine.send_monitor_command("mouse_button 1")
        time.sleep(0.2)
        machine.send_monitor_command("mouse_button 0")
        # 272 == BTN_LEFT.
        journal("pointer button: code=272 pressed=true")
        journal("pointer button: code=272 pressed=false")

    with subtest("wheel events are survivable with nothing under the cursor"):
        # HMP bitmask: 8 = wheel up, 16 = wheel down. Nothing is focused, so the
        # scroll is dropped; this only checks that's harmless.
        machine.send_monitor_command("mouse_button 8")
        machine.send_monitor_command("mouse_button 0")
        machine.send_monitor_command("mouse_button 16")
        machine.send_monitor_command("mouse_button 0")
        ipc("settle 300")

    with subtest("a touch puts the cursor away again"):
        ipc("tap 360 1080")
        ipc("settle 400")
        after_touch = light_px("03-after-touch")
        print(f"light px after touch: {after_touch}")
        assert (
            after_touch <= baseline + 20
        ), f"cursor still drawn after a touch: {baseline} -> {after_touch} light px"

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
