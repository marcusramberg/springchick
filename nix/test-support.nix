# Shared scaffolding for the springchick NixOS VM tests.
#
# Every test boots the same machine: the springchick module, a virtio-gpu DRM
# device with software (llvmpipe) GL, and greetd autologin of `tester` into the
# shipped session. `mkTest` captures that boilerplate so a test file only states
# what is specific to it — its name, the packages it launches, and its script.
#
# All tests run at a phone-shaped resolution and DPI (see `phone` below) so the
# compositor is exercised in the portrait geometry it actually ships on, not the
# 1280x800 landscape QEMU defaults to.
{ self, pkgs }:

let
  # The target device profile shared by every test.
  #   width/height — physical output pixels, forced via the virtio-gpu EDID.
  #   dpi          — output scale advertised to clients (config.toml [main].dpi).
  # 720x1440 keeps llvmpipe fast while staying a true portrait phone aspect; at
  # dpi 2.0 that is 360x720 *logical* px — a realistic phone logical size, and a
  # deliberate step down from the compositor's 3.0 default (which at 720px wide
  # would leave clients a cramped 240 logical px).
  phone = {
    width = 720;
    height = 1440;
    dpi = 2.0;
  };

  mkTest =
    {
      name,
      testScript,
      # OCR (tesseract) for tests that read text off a screenshot.
      enableOCR ? false,
      # Python packages for the test driver, e.g. `p: [ p.pillow ]` for a test
      # that inspects screenshot pixels rather than just saving them.
      extraPythonPackages ? (_: [ ]),
      # Extra packages on the machine's PATH (apps the test launches).
      packages ? [ ],
      # Device profile; override width/height/dpi per test if ever needed.
      resolution ? phone,
      memorySize ? 2048,
      cores ? 2,
      # Escape hatch for test-specific machine config, merged over the base.
      extraMachineConfig ? { },
    }:
    pkgs.testers.runNixOSTest {
      inherit
        name
        enableOCR
        extraPythonPackages
        testScript
        ;

      nodes.machine =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        {
          # extraMachineConfig is merged as a module (deep-merged by the module
          # system), so a test can override or add to the base below.
          imports = [
            self.nixosModules.springchick
            extraMachineConfig
          ];

          programs.springchick.enable = true;
          # Only [main].dpi; everything else stays at the compiled-in defaults.
          programs.springchick.config = ''
            [main]
            dpi = ${toString resolution.dpi}
          '';

          # A virtio-gpu PCI device gives the guest a real DRM node for the DRM
          # backend to master — the same path as on-device. edid=on + xres/yres
          # advertise the phone resolution as the connector's preferred mode,
          # which is what the DRM backend's find_output() picks.
          #
          # mkBefore is load-bearing: on aarch64 the NixOS test driver itself
          # prepends a bare `-device virtio-gpu-pci` (default 1280x800) for
          # screenshots, so without ordering ours becomes card1 and the compositor
          # opens card0 at 1280x800. mkBefore puts ours first → it is card0.
          virtualisation.qemu.options = lib.mkBefore [
            "-vga none"
            "-device virtio-gpu-pci,edid=on,xres=${toString resolution.width},yres=${toString resolution.height}"
          ];
          boot.initrd.kernelModules = [ "virtio_gpu" ];

          # No host GPU in CI: force mesa's software rasterizer, pinned on the
          # service so it survives regardless of how the session wrapper imports
          # the environment.
          hardware.graphics.enable = true;
          systemd.user.services.springchick.environment = {
            LIBGL_ALWAYS_SOFTWARE = "1";
            GALLIUM_DRIVER = "llvmpipe";
            # The tests assert on the compositor's `springchick::debug`-target
            # lines (state changes, decoration policy, xdg-decoration). Those are
            # debug! now (kept out of the default info log), so raise just that
            # target here — not `springchick::perf`, which fires per frame.
            RUST_LOG = "info,springchick::debug=debug";
          };

          # Autologin the shipped session (springchick-session →
          # springchick.service) via greetd — the same entry point the display
          # manager uses on-device. default_session is mandatory; point it at
          # the same command so a session exit just relaunches.
          services.greetd = {
            enable = true;
            settings = {
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
            # A logind seat with a graphical session hands over DRM master and
            # libinput; greetd's PAM session provides it.
            extraGroups = [
              "video"
              "input"
            ];
          };

          environment.systemPackages = packages;
          # A font is required or many clients abort before mapping a window.
          fonts.packages = [ pkgs.dejavu_fonts ];

          virtualisation.memorySize = memorySize;
          virtualisation.cores = cores;
        };
    };
in
{
  inherit mkTest phone;
}
