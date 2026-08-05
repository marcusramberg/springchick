# Boot-smoke VM test for springchick.
#
# Boots the springchick module on the shared phone-shaped VM (see
# nix/test-support.nix), autologins the shipped session via greetd, and asserts
# the compositor comes up on the real device backend:
#   - springchick.service reaches active (its Type=notify READY fired, which
#     means the DRM master was taken and the first frame rendered);
#   - a Wayland socket is published;
#   - a real catalog client (foot) has its xdg app_id resolved against the
#     catalog instead of staying an `unknown_N` placeholder;
#   - the journal is free of panics.
#
# This is the milestone-1 gate: it proves the DRM + GBM + EGL/GLES stack the
# device backend uses actually runs headless (over llvmpipe) in CI.
#
# Build for the host arch (cross-building the x86_64 guest on an aarch64 host
# runs the whole release tree under qemu-user emulation, which crashes rustc).
# Run:  nix build .#checks.aarch64-linux.vm-boot -L   (or x86_64-linux)
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-boot";

  # A real catalog client to launch: foot ships foot.desktop (catalog id "foot")
  # and sets its wayland app_id to "foot", so the compositor should match it.
  packages = [ pkgs.foot ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    # greetd autologins `tester`; wait for its systemd user manager, then for
    # the compositor service to go active. Active == Type=notify READY fired,
    # i.e. DRM master taken + first frame rendered over llvmpipe.
    uid = machine.succeed("id -u tester").strip()
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )

    # The compositor published its Wayland socket.
    machine.wait_until_succeeds(f"ls /run/user/{uid}/springchick-*.lock", timeout=30)

    machine.screenshot("springchick-boot")

    # app_id resolution: launch foot and assert the compositor matches its xdg
    # app_id against the catalog instead of leaving it a `unknown_N` placeholder.
    # Clients set app_id after the toplevel maps, so before the app_id_changed
    # fix the stored id stayed unknown and this log line never appeared.
    socket = machine.succeed(
        "basename $(ls /run/user/1000/springchick-*.lock) .lock"
    ).strip()
    machine.succeed(
        "systemd-run --user -M tester@.host --collect "
        f"--setenv=WAYLAND_DISPLAY={socket} "
        "${pkgs.foot}/bin/foot -e sleep 30"
    )
    machine.wait_until_succeeds(
        "journalctl -b _SYSTEMD_USER_UNIT=springchick.service "
        "| grep -F 'toplevel app_id resolved' | grep -F 'app_id=foot'",
        timeout=30,
    )
    machine.screenshot("springchick-foot")

    # No panic / crash on the way up. Match real crash signatures only — a bare
    # 'panic' also hits the kernel cmdline (`panic=1`) and the virtio-gpu
    # `drm panic` planes, both benign.
    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
