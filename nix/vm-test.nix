# Boot-smoke VM test for springchick.
#
# Boots a NixOS VM with the springchick module, a virtio-gpu DRM device and
# software (llvmpipe) GL, autologins the shipped `springchick-session` via
# greetd, and asserts the compositor comes up on the real device backend:
#   - springchick.service reaches active (its Type=notify READY fired, which
#     means the DRM master was taken and the first frame rendered);
#   - a Wayland socket is published;
#   - the journal is free of panics.
#
# This is the milestone-1 gate: it proves the DRM + GBM + EGL/GLES stack the
# device backend uses actually runs headless (over llvmpipe) in CI, which is the
# risky part. Client-render and logic assertions build on top of it later.
#
# Build for the host arch (cross-building the x86_64 guest on an aarch64 host
# runs the whole release tree under qemu-user emulation, which crashes rustc).
# Run:  nix build .#checks.aarch64-linux.vm-boot -L   (or x86_64-linux)
{ self, pkgs }:

pkgs.testers.runNixOSTest {
  name = "springchick-boot";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [ self.nixosModules.springchick ];

      programs.springchick.enable = true;

      # A virtio-gpu PCI device gives the guest a real DRM node (/dev/dri/card0)
      # for the DRM backend to master — the same code path as on-device, not a
      # test shim. `-vga none` drops the default emulated VGA so virtio-gpu is
      # the only card springchick can pick.
      virtualisation.qemu.options = [
        "-vga none"
        "-device virtio-gpu-pci"
      ];
      boot.initrd.kernelModules = [ "virtio_gpu" ];

      # No host GPU in CI: force mesa's software rasterizer. Pinned on the
      # compositor service directly so it survives regardless of how the session
      # wrapper imports the environment.
      hardware.graphics.enable = true;
      systemd.user.services.springchick.environment = {
        LIBGL_ALWAYS_SOFTWARE = "1";
        GALLIUM_DRIVER = "llvmpipe";
      };

      # Autologin the shipped session (springchick-session → springchick.service)
      # via greetd — the same entry point the display manager uses on-device.
      services.greetd = {
        enable = true;
        settings = {
          # initial_session autologins `tester` into the shipped session once at
          # boot. default_session is mandatory (greetd refuses to start without
          # it); point it at the same session so a session exit just relaunches.
          initial_session = {
            command = "${config.programs.springchick.package}/bin/springchick-session";
            user = "tester";
          };
          default_session = {
            command = "${config.programs.springchick.package}/bin/springchick-session";
            user = "tester";
          };
        };
      };

      users.users.tester = {
        isNormalUser = true;
        # A logind seat with a graphical session is what hands over DRM master
        # and libinput; greetd's PAM session provides it.
        extraGroups = [ "video" "input" ];
      };

      # A real catalog client to launch: foot ships foot.desktop (catalog id
      # "foot") and sets its wayland app_id to "foot", so the compositor should
      # match it. Installed system-wide so its .desktop is on XDG_DATA_DIRS when
      # springchick scans the catalog at startup. A font is required or foot
      # aborts before mapping a window.
      environment.systemPackages = [ pkgs.foot ];
      fonts.packages = [ pkgs.dejavu_fonts ];

      # The screenshot/OCR path is unused at this milestone, but keep the VM
      # small and deterministic.
      virtualisation.memorySize = 2048;
      virtualisation.cores = 2;
    };

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

    # app_id resolution: launch a real catalog client (foot) and assert the
    # compositor matches its xdg app_id against the catalog instead of leaving
    # it a `unknown_N` placeholder — the app_id_changed fix. Clients set app_id
    # after the toplevel maps, so before the fix the stored id stayed unknown
    # and this log line never appeared.
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
