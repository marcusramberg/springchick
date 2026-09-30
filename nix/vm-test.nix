# Boot smoke test: springchick.service goes active (READY = DRM master taken,
# first frame rendered), a socket is published, foot's app_id resolves against
# the catalog, and nothing panics.
#
# Build for the host arch; cross-building crashes rustc under qemu-user.
# Run:  nix build .#checks.aarch64-linux.vm-boot -L   (or x86_64-linux)
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-boot";

  # foot.desktop's id and foot's app_id are both "foot".
  packages = [ pkgs.foot ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    uid = machine.succeed("id -u tester").strip()
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )

    machine.wait_until_succeeds(f"ls /run/user/{uid}/springchick-*.lock", timeout=30)

    machine.screenshot("springchick-boot")

    # app_id arrives after map, so this line proves the retag happened.
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

    # Real crash signatures only: a bare 'panic' matches `panic=1` on the kernel
    # cmdline and virtio-gpu's `drm panic` planes.
    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
