# Decoration policy with a real GTK4 app: the main window gets server-side
# decorations; a modal transient child (set_parent + xdg-dialog hint) is a
# dialog and keeps client-side ones, so its buttons stay on screen. Asserted
# on the `configure toplevel dialog=… decoration=…` log line, then OCR of the
# button labels.
#
# A Qt app is the contrast: Qt negotiates xdg-decoration and goes borderless.
# GTK never creates a decoration object, so it keeps its header regardless.
#
# Run:  nix build .#checks.aarch64-linux.vm-dialog -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # One typelib dir, so bare python3 + pygobject can load GTK4 without
  # wrapGAppsHook.
  giEnv = pkgs.buildEnv {
    name = "gtk4-gi-typelibs";
    paths = [
      pkgs.gtk4 # Gtk/Gdk/Gsk/GdkWayland-4.0
      pkgs.glib.out # GLib/GObject/Gio-2.0 (typelibs live in the `out` output)
      pkgs.pango.out # Pango/PangoCairo-1.0 (typelibs live in the `out` output)
      pkgs.gdk-pixbuf # GdkPixbuf-2.0
      pkgs.graphene # Graphene-1.0
      pkgs.harfbuzz # HarfBuzz-0.0
      pkgs.gobject-introspection # cairo-1.0 and friends
    ];
  };

  pythonEnv = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);

  # A main window, then a modal transient child with two buttons. Big font for
  # OCR.
  dialogPy = pkgs.writeText "gtk-dialog-demo.py" ''
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, GLib, Gdk

    def on_activate(app):
        win = Gtk.ApplicationWindow(application=app, title="dialog-parent")
        win.set_default_size(600, 400)
        win.present()

        css = Gtk.CssProvider()
        css.load_from_string("* { font-size: 48px; }")

        def open_child():
            dlg = Gtk.Window(transient_for=win, modal=True, title="dialog-child")
            Gtk.StyleContext.add_provider_for_display(
                dlg.get_display(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
            )
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
            box.append(Gtk.Button(label="Confirm"))
            box.append(Gtk.Button(label="Cancel"))
            dlg.set_child(box)
            dlg.present()
            return False

        # Late enough to screenshot the parent alone first.
        GLib.timeout_add(4000, open_child)

    app = Gtk.Application(application_id="org.springchick.DialogDemo")
    app.connect("activate", on_activate)
    app.run(None)
  '';

  gtkDialogApp = pkgs.writeShellApplication {
    name = "gtk-dialog-demo";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      export GDK_BACKEND=wayland
      # The cairo renderer is reliable over llvmpipe.
      export GSK_RENDERER=cairo
      exec python3 ${dialogPy}
    '';
  };

  # qdbusviewer's wrapper has qtbase's plugins; qtwayland (xdg-shell and
  # decoration integration) isn't a qttools dep, so add it.
  qtProbe = pkgs.writeShellApplication {
    name = "qt-probe";
    runtimeInputs = [ pkgs.qt6.qttools ];
    text = ''
      export QT_QPA_PLATFORM=wayland
      export QT_PLUGIN_PATH="${pkgs.qt6.qtwayland}/lib/qt-6/plugins''${QT_PLUGIN_PATH:+:$QT_PLUGIN_PATH}"
      exec qdbusviewer
    '';
  };
in
mkTest {
  name = "springchick-dialog";

  enableOCR = true;

  packages = [
    gtkDialogApp
    qtProbe
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    machine.succeed(
        f"systemd-run --user -M tester@.host --collect --unit=gtk-dialog "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v gtk-dialog-demo)"
    )

    # Oracle 1: the main window is asked for server-side decorations (GTK draws
    # its header anyway; this just shows the two get different modes).
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'dialog=false decoration=ServerSide'", timeout=60
    )
    machine.screenshot("01-main-window")

    # Oracle 2: the child is a dialog and keeps client-side decorations.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'dialog=true decoration=ClientSide'", timeout=60
    )

    # Oracle 3: both buttons are legible on screen.
    machine.wait_for_text(r"Confirm")
    machine.wait_for_text(r"Cancel")
    machine.screenshot("02-child-dialog")

    # Contrast: Qt accepts server-side and drops its titlebar.
    # `xdg-decoration negotiated` only fires from new_decoration, which GTK
    # never reaches.
    machine.succeed(
        f"systemd-run --user -M tester@.host --collect --unit=qt-probe "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v qt-probe)"
    )
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'xdg-decoration negotiated'", timeout=60
    )
    machine.screenshot("03-qt-borderless")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
