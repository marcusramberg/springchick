# Arrange mode: a long press on empty background engages it (launching
# nothing); a press then lifts an icon with no second hold; dropping it
# reorders the grid, persisted to state.toml; dropping on the dock pins.
# Arrange keeps Home as Home, so this asserts on the `arrange` log lines and
# state.toml.
#
# Run:  nix build .#checks.aarch64-linux.vm-arrange -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # Never run, but the exec must be valid for the catalog to accept them.
  gridApp =
    name:
    pkgs.makeDesktopItem {
      inherit name;
      desktopName = name;
      exec = "${pkgs.foot}/bin/foot --app-id=${name} -e sleep 6000";
    };
in
mkTest {
  name = "springchick-arrange";

  homePages = [
    [
      "aaa"
      "bbb"
      "ccc"
    ]
  ];

  packages = [
    pkgs.foot
    pkgs.python3
    (gridApp "aaa")
    (gridApp "bbb")
    (gridApp "ccc")
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"
    STATE = "/home/tester/.config/springchick/state.toml"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    # Mirrored from crates/sc-layout/src/lib.rs. An icon is only ~0.07H tall, so
    # eyeballed fractions miss.
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
        """Centre x of grid column i (0-based)."""
        return int(W * H_MARGIN + i * CELL_W + CELL_W / 2)

    def row(r):
        """Centre y of the icon in grid row r — not the cell centre: the icon
        sits above its label, so the two differ by half the label height."""
        cell_y = H * TOP_PAD + r * CELL_H
        return int(cell_y + (CELL_H - ICON - CELL_H * LABEL_FRAC) / 2 + ICON / 2)

    ROW0 = row(0)
    DOCK = int(H * 0.91)  # dock band spans 0.87H..0.97H

    def order():
        """The first page's app order, read straight from the persisted model."""
        # Not written until the first arrange edit.
        if machine.succeed(f"test -f {STATE} && echo y || echo n").strip() == "n":
            return None
        raw = machine.succeed(
            f"python3 -c \"import tomllib,sys;"
            f"print(' '.join(tomllib.load(open('{STATE}','rb'))['pages'][0]))\""
        ).strip()
        return raw.split()

    # Only three apps, so row 4 is empty background.
    EMPTY = row(4)

    # The hold is a timer: the compositor must keep advancing frames under a
    # perfectly still finger.
    dbg(f"down {col(0)} {EMPTY}")
    machine.sleep(2)
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'arrange engaged'", timeout=15
    )
    dbg("up")
    machine.screenshot("01-arrange-engaged")

    # Drag slot 0 to slot 2, well past the tap slop.
    dbg(f"down {col(0)} {ROW0}")
    dbg(f"move {col(1)} {ROW0}")
    dbg(f"move {col(2)} {ROW0}")
    dbg(f"move {col(2)} {ROW0 + 4}")
    dbg("up")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'arrange drop .* action=Reorder'", timeout=15
    )
    machine.screenshot("02-after-reorder")

    # Arrange consumes the pending launch.
    machine.fail(f"{JOURNAL} | grep -qF 'state changed to App'")

    # Page 0 was [aaa, bbb, ccc]; slot 0 → 2 rotates them.
    after = order()
    assert after is not None, "state.toml was never written after the arrange edit"
    assert after[:3] == ["bbb", "ccc", "aaa"], (
        f"expected page 0 to start [bbb, ccc, aaa] after the drag, got {after[:6]}"
    )

    # Still in arrange, so the press lifts.
    dbg(f"down {col(0)} {ROW0}")
    dbg(f"move {col(0)} {int(0.5 * H)}")
    dbg(f"move {col(1)} {DOCK}")
    dbg(f"move {col(1)} {DOCK + 2}")
    dbg("up")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'arrange drop .* action=Pin'", timeout=15
    )
    machine.screenshot("03-after-pin")

    # bbb is in slot 0 after the reorder.
    pinned = machine.succeed(
        f"python3 -c \"import tomllib;"
        f"print(' '.join(tomllib.load(open('{STATE}','rb')).get('dock',[])))\""
    ).strip().split()
    assert pinned == ["bbb"], f"expected bbb pinned to the dock, dock={pinned}"
    # Pinned apps leave the grid.
    assert "bbb" not in order(), f"bbb is pinned but still on the grid: {order()[:6]}"

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
