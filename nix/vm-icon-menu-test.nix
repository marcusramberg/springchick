# Icon context menu + multi-instance test.
#
# Covers the path that used to be impossible: a second window of an app that is
# already running. Tapping an icon raises the app, so "new window" needs its own
# affordance — the menu a long press on an icon opens.
#
# It exercises:
#   - long-press on a grid icon opens its menu (and does NOT launch the app);
#   - a stopped app's menu offers "New window" but not "Open"/"Close";
#   - the menu's "New window" row starts an instance;
#   - doing it again while the app is running starts a *second* one, both of
#     which are real, distinct toplevels;
#   - tapping the icon afterwards raises rather than launching a third;
#   - a window launched through a `Terminal=true`-style wrapper keeps its own
#     identity instead of being tagged with the terminal's app id.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-icon-menu -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # A plain app. `--app-id` deliberately does *not* match the .desktop id: the
  # shell must tag the window from the launch, not from what the client calls
  # itself, which is exactly what breaks terminal wrappers and PWA runners.
  termApp = pkgs.makeDesktopItem {
    name = "aaa";
    desktopName = "aaa";
    exec = "${pkgs.foot}/bin/foot --app-id=foot -e sleep 6000";
  };
in
mkTest {
  name = "springchick-icon-menu";

  packages = [
    pkgs.foot
    pkgs.python3
    termApp
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def go_home():
        """Return to Home from a running app.

        A *tap* on the home bar does not do it — in an app the bar starts a
        grab, and a still release leaves the app where it was. Home needs an
        upward bar swipe past HOME_MIN_PROGRESS (0.35 of the screen height);
        1418 -> 700 is ~0.50, well clear of the switcher-reveal band below it.
        """
        dbg(f"swipe {int(W / 2)} {int(H * 0.985)} {int(W / 2)} {int(H * 0.486)} 400")
        dbg("settle 3000")
        machine.wait_until_succeeds(
            f"{JOURNAL} | grep -qF 'state changed to Home'", timeout=15
        )

    def foot_count():
        """How many foot processes are running (one per opened window)."""
        return int(machine.succeed("pgrep -c -x foot || true").strip() or 0)

    # Grid geometry in physical output pixels, mirrored from the layout
    # constants in crates/sc-layout/src/lib.rs — see vm-arrange for the details.
    W = ${toString phone.width}
    H = ${toString phone.height}
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

    ROW0 = row(0)

    # Menu geometry, mirrored from crates/sc-layout/src/menu.rs. The panel hangs
    # below a top-row icon, so its rows run downward from the anchor.
    PANEL_W = W * 0.52
    ITEM_H = H * 0.045
    PAD = H * 0.008
    GAP = H * 0.035
    MENU_X = int(min(max(col(0) - PANEL_W / 2, W * 0.04), W - W * 0.04 - PANEL_W) + PANEL_W / 2)

    def menu_row(i):
        """Centre y of menu row i for a menu anchored on the first grid row."""
        return int(ROW0 + GAP + PAD + i * ITEM_H + ITEM_H / 2)

    # --- Long-press on an icon opens its menu, and launches nothing ---
    dbg(f"down {col(0)} {ROW0}")
    machine.sleep(2)
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'icon menu opened app_id=aaa'", timeout=15
    )
    dbg("up")
    machine.screenshot("01-menu-open")
    machine.fail(f"{JOURNAL} | grep -qF 'state changed to App'")
    assert foot_count() == 0, "the long-press must not have launched the app"

    # A stopped app has no window to open or close, so its menu leads with
    # "New window" — row 0.
    dbg(f"tap {MENU_X} {menu_row(0)}")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'icon menu action app_id=aaa action=NewWindow'", timeout=15
    )
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to App'", timeout=30)
    machine.wait_until_succeeds("pgrep -x foot", timeout=30)
    machine.screenshot("02-first-window")

    # --- The window is tagged from the launch, not from its own app_id ---
    # foot reports `foot`; the shell must have kept `aaa`, or tap-to-raise would
    # land on the wrong app and a second tap would spawn a duplicate.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'toplevel attributed to launch.*app_id=aaa'", timeout=15
    )

    # --- Home, then a second window from the same icon ---
    go_home()
    dbg(f"down {col(0)} {ROW0}")
    machine.sleep(2)
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'icon menu opened app_id=aaa .*running=1'", timeout=15
    )
    dbg("up")
    machine.screenshot("03-menu-running")

    # Running now, so the rows are Open / New window / Close / Remove — "New
    # window" has moved to row 1. (With two or more windows the leading row
    # becomes one row *per window*, listed by title.)
    dbg(f"tap {MENU_X} {menu_row(1)}")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'icon menu action app_id=aaa action=NewWindow'", timeout=15
    )
    machine.wait_until_succeeds("test $(pgrep -c -x foot || true) -eq 2", timeout=30)
    machine.screenshot("04-second-window")

    # --- A plain tap raises instead of launching a third ---
    go_home()
    dbg(f"tap {col(0)} {ROW0}")
    dbg("settle 3000")
    assert foot_count() == 2, (
        f"tapping a running app's icon must raise, not launch; foot count {foot_count()}"
    )
  '';
}
