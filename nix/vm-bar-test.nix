# sc-bar holds its strip at the top, a tap on its right brings the control
# sheet down, a tile reaches the compositor, a swipe up puts the sheet away, and a
# shortcut launches its app.
#
# Run:  nix build .#checks.aarch64-linux.vm-bar -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-bar";

  # Icons behind the sheet, so the screenshot shows its blur.
  packages = [
    pkgs.foot
    pkgs.libnotify
  ];
  homePages = [
    [
      "foot"
      "footclient"
      "foot-server"
    ]
  ];

  extraMachineConfig =
    { lib, ... }:
    {
      programs.springchick.bar.enable = lib.mkForce true;
      programs.springchick.settings.bar.long_press.wifi = "touch /tmp/long-press";

      # Something for the sheet to show: a simulated Wi-Fi radio, a power
      # profile and a sink.
      networking.networkmanager.enable = true;
      boot.kernelModules = [ "mac80211_hwsim" ];
      services.power-profiles-daemon.enable = true;
      services.pipewire = {
        enable = true;
        pulse.enable = true;
      };
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def layers(pattern):
        return f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc layers | grep -q '{pattern}'"

    # 28 logical px at dpi 2, and the usable area starts below it.
    machine.wait_until_succeeds(layers("ns=bar layer=bottom .* excl=28"), timeout=30)
    machine.wait_until_succeeds(layers("usable=0,56"), timeout=30)
    dbg("settle 2000")
    machine.screenshot("bar")

    dbg("tap 600 20")
    machine.wait_until_succeeds(layers("ns=bar-sheet layer=overlay"), timeout=30)
    machine.sleep(2)
    dbg("settle 2000")
    machine.screenshot("control-center")

    # The first tile, the rotation lock: springchick's own, so always there,
    # and on until tapped.
    assert "disabled" in dbg("rotation")
    dbg("tap 124 618")
    machine.wait_until_succeeds(
        f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc rotation | grep -q enabled", timeout=30
    )

    # The power tile turns the sheet to its power page; a tap outside turns it back.
    dbg("tap 281 618")
    machine.sleep(2)
    machine.screenshot("power")
    dbg("tap 360 1380")
    machine.sleep(1)
    machine.succeed(layers("ns=bar-sheet"))

    dbg("tswipe 600 1300 600 300 300")
    machine.wait_until_succeeds(f"! {layers('ns=bar-sheet')}", timeout=30)

    # A finger resting on Wi-Fi runs its long-press command and closes the sheet.
    dbg("tap 600 20")
    machine.wait_until_succeeds(layers("ns=bar-sheet layer=overlay"), timeout=30)
    machine.sleep(2)
    dbg("tswipe 269 315 269 315 2000")
    machine.wait_until_succeeds("ls /tmp/long-press", timeout=30)
    machine.wait_until_succeeds(f"! {layers('ns=bar-sheet')}", timeout=30)
    machine.sleep(3)

    # A notification drops in as a banner, which goes back up by itself.
    def notify(app, summary, body, flags=""):
        machine.succeed(
            "su - tester -c 'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus"
            f" notify-send {flags} -a {app} -h string:desktop-entry:{app} \"{summary}\" \"{body}\""
            " >/dev/null 2>&1 &'"
        )

    notify("foot", "Ada Lovelace", "Shall we meet at the engine at four?")
    machine.wait_until_succeeds(layers("ns=bar-banner layer=overlay"), timeout=30)
    machine.sleep(2)
    machine.screenshot("banner")
    machine.wait_until_succeeds(f"! {layers('ns=bar-banner')}", timeout=30)

    # Pulled down on the left, the sheet is Notification Center: one app's
    # notifications stacked, another's below. A tap on a card opens it.
    notify("footclient", "Charles Babbage", "The cards are punched.", "--wait")
    notify("foot", "Ada Lovelace", "Bring the notes.")
    machine.wait_until_succeeds(f"! {layers('ns=bar-banner')}", timeout=30)
    dbg("tap 100 20")
    machine.wait_until_succeeds(layers("ns=bar-sheet layer=overlay"), timeout=30)
    machine.sleep(3)
    machine.screenshot("notifications")
    machine.succeed("pgrep -x notify-send")
    dbg("tap 360 624")
    machine.wait_until_succeeds(f"! {layers('ns=bar-sheet')}", timeout=30)
    machine.fail("journalctl -b _SYSTEMD_USER_UNIT=sc-bar.service | grep -F 'would not open'")
    # Its sender hears at once, not with the bus's next message.
    machine.wait_until_fails("pgrep -x notify-send", timeout=10)
    dbg("action home")
    machine.sleep(3)

    # An app given a shortcut gets a tile on the sheet, which launches it.
    assert "foot" in dbg("shortcuts foot")
    dbg("tap 600 20")
    machine.wait_until_succeeds(layers("ns=bar-sheet layer=overlay"), timeout=30)
    machine.sleep(2)
    machine.screenshot("shortcuts")
    launches = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service | grep -cF 'app_id=foot'"
    before = int(machine.execute(launches)[1])
    # After the dark mode tile.
    dbg("tap 281 933")
    machine.wait_until_succeeds(f"test $({launches}) -gt {before}", timeout=30)
    machine.succeed(f"! {layers('ns=bar-sheet')}")

    machine.succeed("command -v bluetui")
    # Nothing above may leave the compositor drawing frames for ever.
    machine.wait_until_succeeds(
        f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc busy | grep -q idle", timeout=30
    )

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
