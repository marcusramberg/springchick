# Shared scaffolding for the VM tests: the springchick module on a virtio-gpu
# DRM device with llvmpipe, greetd autologin of `tester`, and a portrait phone
# geometry (see `phone`).
{ self, pkgs }:

let
  # 720x1440 keeps llvmpipe fast; at dpi 2.0 that is a realistic 360x720
  # logical. The compositor's default of 3.0 would leave only 240 wide.
  phone = {
    width = 720;
    height = 1440;
    dpi = 2.0;
  };

  mkTest =
    {
      name,
      testScript,
      enableOCR ? false,
      # e.g. `p: [ p.pillow ]` for pixel checks.
      extraPythonPackages ? (_: [ ]),
      # Apps the test launches.
      packages ? [ ],
      resolution ? phone,
      memorySize ? 2048,
      cores ? 2,
      # Merged over the base machine config.
      extraMachineConfig ? { },
      # Home pages as lists of .desktop ids. A fresh install's home is empty, so any
      # test that presses a grid icon must seed it.
      homePages ? [ ],
    }:
    let
      seedState = homePages != [ ];

      # A file, not echoed: multi-line TOML can't sit in the one-line Python string.
      stateFile = pkgs.writeText "springchick-seed-state.toml" ''
        pages = [${
          pkgs.lib.concatMapStringsSep ", " (
            page: "[" + pkgs.lib.concatMapStringsSep ", " (app: ''"${app}"'') page + "]"
          ) homePages
        }]
        dock = []
      '';

      # Copied in after boot plus a service restart: the home dir doesn't reliably
      # exist early enough for tmpfiles.
      seedPrelude = pkgs.lib.optionalString seedState ''
        machine.wait_for_unit("multi-user.target")
        machine.wait_until_succeeds(
            "systemctl --user -M tester@.host is-active springchick.service", timeout=90
        )
        machine.succeed("mkdir -p /home/tester/.config/springchick")
        machine.succeed(
            "install -o tester -g users -m 0644"
            " /etc/springchick-seed-state.toml"
            " /home/tester/.config/springchick/state.toml"
        )
        machine.succeed("chown -R tester:users /home/tester/.config")
        machine.succeed("systemctl --user -M tester@.host restart springchick.service")
        machine.wait_until_succeeds(
            "systemctl --user -M tester@.host is-active springchick.service", timeout=90
        )
        machine.wait_until_succeeds("ls /run/user/1000/springchick-ipc.sock", timeout=30)
      '';
    in
    pkgs.testers.runNixOSTest {
      inherit
        name
        enableOCR
        extraPythonPackages
        ;
      testScript = seedPrelude + testScript;

      nodes.machine =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        {
          imports = [
            self.nixosModules.springchick
            extraMachineConfig
          ];

          programs.springchick.enable = true;
          programs.springchick.config = ''
            [main]
            dpi = ${toString resolution.dpi}
          '';

          # virtio-gpu with an EDID advertising the phone mode, which find_output()
          # picks. mkBefore matters: on aarch64 the driver prepends its own
          # virtio-gpu-pci, and ours must be card0.
          virtualisation.qemu.options = lib.mkBefore [
            "-vga none"
            "-device virtio-gpu-pci,edid=on,xres=${toString resolution.width},yres=${toString resolution.height}"
          ];
          boot.initrd.kernelModules = [ "virtio_gpu" ];

          # No host GPU: force llvmpipe, pinned on the service.
          hardware.graphics.enable = true;
          systemd.user.services.springchick.environment = {
            LIBGL_ALWAYS_SOFTWARE = "1";
            GALLIUM_DRIVER = "llvmpipe";
            # The tests grep `springchick::debug` lines; not `perf`, which is per frame.
            RUST_LOG = "info,springchick::debug=debug";
          };

          # The same entry point as on-device. default_session is mandatory; a session
          # exit just relaunches.
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
            extraGroups = [
              "video"
              "input"
            ];
          };

          environment.systemPackages = packages;
          environment.etc = lib.mkIf seedState {
            "springchick-seed-state.toml".source = stateFile;
          };
          # Many clients abort without a font.
          fonts.packages = [ pkgs.dejavu_fonts ];

          virtualisation.memorySize = memorySize;
          virtualisation.cores = cores;
        };
    };
in
{
  inherit mkTest phone;
}
