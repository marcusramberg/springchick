# README screenshots at FP5 resolution. Each scene starts from a fresh
# compositor, so no scene depends on dismissing the last one.
#
# Run:  just screenshots   (builds this check and copies result/*.png to assets/)
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # Catppuccin Macchiato surface1 -> crust, matching extras/waybar.
  wallpaper = pkgs.runCommand "springchick-wallpaper.png" { nativeBuildInputs = [ pkgs.imagemagick ]; } ''
    magick -size 1224x2700 gradient:'#494d64'-'#181926' $out
  '';

  apps = with pkgs; [
    gnome-calculator
    gnome-clocks
    gnome-text-editor
    gnome-characters
    nautilus
    gnome-console
    gnome-weather
    loupe
    baobab
    gnome-system-monitor
    gnome-font-viewer
    gnome-maps
    snapshot
    gnome-music
    gnome-calendar
  ];
in
mkTest {
  name = "springchick-screenshots";

  resolution = {
    width = 1224;
    height = 2700;
    dpi = 3.0;
  };
  memorySize = 4096;
  cores = 4;

  # Rows 4-5 stay empty: arrange is entered by a long press there.
  homePages = [
    [
      "org.gnome.Calculator"
      "org.gnome.clocks"
      "org.gnome.Weather"
      "org.gnome.Calendar"
      "org.gnome.Maps"
      "org.gnome.Music"
      "org.gnome.Snapshot"
      "org.gnome.Loupe"
      "org.gnome.Characters"
      "org.gnome.font-viewer"
      "org.gnome.baobab"
      "org.gnome.SystemMonitor"
    ]
  ];
  dock = [
    "org.gnome.Nautilus"
    "org.gnome.TextEditor"
    "org.gnome.Console"
  ];

  extraMachineConfig = {
    # Most of these are DBusActivatable, launched through `gapplication`.
    services.dbus.packages = apps;
    fonts.packages = [ pkgs.nerd-fonts.jetbrains-mono ];
    # Matches the Macchiato bar.
    programs.dconf.profiles.user.databases = [
      { settings."org/gnome/desktop/interface".color-scheme = "prefer-dark"; }
    ];
    # A null sink gives wpctl something to turn up for the volume OSD.
    services.pipewire = {
      enable = true;
      pulse.enable = true;
      wireplumber.enable = true;
      extraConfig.pipewire."10-null-sink"."context.objects" = [
        {
          factory = "adapter";
          args = {
            "factory.name" = "support.null-audio-sink";
            "node.name" = "null-sink";
            "media.class" = "Audio/Sink";
            "audio.position" = "FL,FR";
          };
        }
      ];
    };
  };

  packages = apps ++ [
    pkgs.glib.bin
    pkgs.swaybg
    pkgs.waybar
    pkgs.adwaita-icon-theme
  ];

  testScript = ''
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"
    W, H = 1224, 2700

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def fresh():
        """Restart the compositor (dropping its clients); bring back wallpaper and bar."""
        machine.succeed(f"rm -f {IPC_SOCK}")
        machine.succeed("systemctl --user -M tester@.host restart springchick.service")
        machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)
        sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()
        machine.succeed(
            "systemd-run --user -M tester@.host --collect "
            f"--setenv=WAYLAND_DISPLAY={sock} "
            "${pkgs.swaybg}/bin/swaybg -m fill -i ${wallpaper}"
        )
        machine.succeed(
            "systemd-run --user -M tester@.host --collect "
            f"--setenv=WAYLAND_DISPLAY={sock} "
            "${pkgs.waybar}/bin/waybar -c ${../extras/waybar}/springchick.jsonc"
            " -s ${../extras/waybar}/springchick.css"
        )
        machine.sleep(4)  # both map their layers

    def shot(name):
        # Best effort: the deck takes seconds to settle under llvmpipe.
        machine.execute(f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc settle 8000")
        machine.screenshot(name)

    # Mirrored from crates/sc-layout/src/lib.rs; see vm-arrange.
    H_MARGIN, TOP_PAD = 0.04, 0.04
    BAR_H, DOCK_H, DOTS_H = 0.03, 0.10, 0.02
    COLS, ROWS = 4, 6
    ICON_FRAC, LABEL_FRAC = 0.62, 0.18
    CELL_W = W * (1 - 2 * H_MARGIN) / COLS
    CELL_H = H * (1 - BAR_H - DOCK_H - DOTS_H - TOP_PAD) / ROWS
    ICON = CELL_W * ICON_FRAC

    def col(i):
        return int(W * H_MARGIN + i * CELL_W + CELL_W / 2)

    def row(r):
        cell_y = H * TOP_PAD + r * CELL_H
        return int(cell_y + (CELL_H - ICON - CELL_H * LABEL_FRAC) / 2 + ICON / 2)

    def long_press(x, y, log):
        dbg(f"down {x} {y}")
        machine.sleep(2)
        machine.wait_until_succeeds(f"{JOURNAL} | grep -qF '{log}'", timeout=15)
        dbg("up")

    machine.wait_for_unit("multi-user.target")

    fresh()
    shot("home")

    fresh()
    long_press(col(1), row(0), "icon menu opened")
    shot("icon-menu")

    fresh()
    long_press(col(1), row(4), "arrange engaged")
    shot("arrange")

    fresh()
    dbg(f"swipe {int(W * 0.85)} {int(H * 0.5)} {int(W * 0.15)} {int(H * 0.5)} 400")
    shot("library")

    fresh()
    for app in ["org.gnome.Weather", "org.gnome.clocks", "org.gnome.Calculator"]:
        dbg(f"launch {app}")
        machine.wait_until_succeeds(
            f"{JOURNAL} | grep -F 'state changed to App {{' | grep -qF '\"{app}\"'", timeout=60
        )
        machine.sleep(3)  # first frames under llvmpipe
    WPCTL = "runuser -u tester -- env XDG_RUNTIME_DIR=/run/user/1000 wpctl"
    machine.wait_until_succeeds(f"{WPCTL} get-volume @DEFAULT_SINK@ | grep -q Volume", timeout=30)
    machine.succeed(f"{WPCTL} set-volume @DEFAULT_SINK@ 0.6")
    dbg("action volume-up")
    # The OSD is gone 1.8 s after the press: no polling waits here.
    machine.execute(f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc settle 300")
    machine.screenshot("volume")
    machine.sleep(3)  # let the OSD fade

    # Slow bar swipe into the switcher band, as in vm-switcher.
    dbg(f"swipe {W // 2} {int(H * 0.985)} {W // 2} {int(H * 0.7125)} 800")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to Switcher'", timeout=15)
    shot("switcher")
  '';
}
