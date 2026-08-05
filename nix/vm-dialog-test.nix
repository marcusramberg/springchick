# xdg-dialog decoration-policy test with a real GTK app.
#
# On the shared phone-shaped VM (see nix/test-support.nix), launches a small
# GTK4 app that maps a main window and, half a second later, a modal child
# window (transient_for = main). This exercises the dialog-detection path end to
# end:
#   - the MAIN window is a plain top-level app -> server-side (borderless)
#     decorations;
#   - the CHILD window sets a parent (set_parent) AND the xdg-dialog modal hint,
#     so `is_dialog` flips it to client-side decorations, which is what keeps a
#     toolkit's action buttons (a GTK dialog's OK/Cancel) on screen.
#
# The compositor logs `configure toplevel dialog=<bool> decoration=<mode>` on
# every (re)configure, so the journal is the assertion oracle for the policy;
# OCR then reads the two button labels off a screenshot to prove they are
# actually visible (not clipped by server-side decorations or off the bottom).
#
# It then launches a Qt app as a contrast: Qt speaks xdg-decoration, so with
# prefer_no_csd the compositor hands it server-side and it drops its own
# titlebar (goes borderless). GTK never creates an xdg-decoration object at all,
# so it keeps its header regardless — this is not a springchick bug but GTK's
# CSD-only behavior, and the Qt case proves the compositor's server-side path
# works for a toolkit that actually negotiates.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-dialog -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # Symlink every GObject-introspection typelib GTK4 needs into one directory so
  # a bare `python3` (with pygobject) can `import gi; require_version('Gtk','4.0')`
  # by pointing GI_TYPELIB_PATH at it — no wrapGAppsHook machinery required.
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

  # Maps a main ApplicationWindow, then a modal transient child carrying two
  # labelled buttons. GTK4 issues set_parent for the transient and the xdg-dialog
  # modal hint for the modal — either alone makes the compositor treat the child
  # as a dialog. The big font is for OCR: the test reads the button labels off a
  # screenshot to prove they actually landed on screen.
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

        # Delayed enough that the test can screenshot the bare (borderless,
        # server-side) parent before the modal child maps over it.
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
      # GTK4 defaults to a GL renderer; force the cairo software renderer so it
      # is reliable over llvmpipe in CI.
      export GSK_RENDERER=cairo
      exec python3 ${dialogPy}
    '';
  };

  # A small Qt Widgets app (draws a real menubar/titlebar under X, but honors
  # xdg-decoration on Wayland). qdbusviewer ships in qttools. Its wrapper already
  # puts qtbase's plugins (incl. the wayland QPA platform, libqwayland.so) on
  # QT_PLUGIN_PATH; qtwayland — which carries the xdg-shell + decoration shell
  # integration — is not a qttools dep, so add it here.
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

  # OCR (tesseract) so the test can read the dialog's button labels off a
  # screenshot and prove they are visible on screen.
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

    # Launch the GTK app in the tester's session, pointed at the compositor's
    # Wayland socket. --collect reaps the transient unit when it exits.
    machine.succeed(
        f"systemd-run --user -M tester@.host --collect --unit=gtk-dialog "
        f"--setenv=WAYLAND_DISPLAY={sock} $(command -v gtk-dialog-demo)"
    )

    # Oracle 1: the main window is a plain top-level app -> the compositor
    # requests server-side (borderless) decorations for it. (GTK ignores that and
    # draws its own header regardless — only the dialog path below is load-bearing
    # for keeping a toolkit's action buttons; this just proves the two toplevels
    # get different decoration modes.) Screenshot it alone; the child's map is
    # delayed (see the app) so this is the parent, not the dialog over it.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'dialog=false decoration=ServerSide'", timeout=60
    )
    machine.screenshot("01-main-window")

    # Oracle 2: the modal transient child is detected as a dialog -> its toolkit
    # keeps its own decorations (client-side), so its action buttons survive.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'dialog=true decoration=ClientSide'", timeout=60
    )

    # Oracle 3: the buttons are actually visible. OCR the screen and require both
    # button labels to be legible — the end-user-visible proof that keeping the
    # dialog client-side (and NOT fullscreen) leaves its action buttons on screen.
    machine.wait_for_text(r"Confirm")
    machine.wait_for_text(r"Cancel")
    machine.screenshot("02-child-dialog")

    # --- Contrast: a Qt app DOES speak xdg-decoration, so it goes borderless ---
    # GTK above kept its header because it never creates an xdg-decoration object.
    # Qt creates one; with prefer_no_csd the compositor answers server-side, so Qt
    # drops its own titlebar. `xdg-decoration negotiated` is emitted only from the
    # new_decoration handler, which GTK never reaches — so seeing it here is proof
    # a real, self-decorating toolkit accepted our server-side decoration.
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
