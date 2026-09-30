# File chooser fit. GTK's own chooser has a minimum width far over a phone's
# ~360 logical px and ignores the configured size; routing the FileChooser
# portal to phrosh (nix/module.nix) fixes it. The oracle is the compositor's
# `toplevel size … oversize=<bool>` line:
#   A: GTK's in-process chooser -> oversize=true (the bug, reproduced)
#   B: the same through the portal -> oversize=false
#   C: a dismissed dialog returns to the app underneath
#   D: closing the app itself goes Home
# Each phase only reads journal lines logged after it started.
#
# Run:  nix build .#checks.aarch64-linux.vm-portal -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # Same typelib bundle as vm-dialog-test.nix.
  giEnv = pkgs.buildEnv {
    name = "gtk4-gi-typelibs";
    paths = [
      pkgs.gtk4
      pkgs.glib.out
      pkgs.pango.out
      pkgs.gdk-pixbuf
      pkgs.graphene
      pkgs.harfbuzz
      pkgs.gobject-introspection
    ];
  };

  pythonEnv = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);

  # The deprecated GtkFileChooserDialog on purpose: GtkFileDialog uses the
  # portal whenever there is one, ignoring GTK_USE_PORTAL=0.
  chooserPy = pkgs.writeText "gtk-chooser-demo.py" ''
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, GLib, Gio

    def on_activate(app):
        win = Gtk.ApplicationWindow(application=app, title="chooser-parent")
        win.set_default_size(300, 400)
        win.present()

        def open_chooser():
            dlg = Gtk.FileChooserDialog(
                title="Pick a file",
                transient_for=win,
                action=Gtk.FileChooserAction.OPEN,
            )
            dlg.add_button("Cancel", Gtk.ResponseType.CANCEL)
            dlg.add_button("Open", Gtk.ResponseType.ACCEPT)
            dlg.present()
            return False

        # Late, so the parent's fitting geometry is logged first.
        GLib.timeout_add(3000, open_chooser)

    app = Gtk.Application(application_id="org.springchick.ChooserDemo")
    app.connect("activate", on_activate)
    app.run(None)
  '';

  # OpenFile answers asynchronously via a Response signal. A one-shot `gdbus
  # call` exits right away, the portal loses its peer, and the picker closes.
  portalPy = pkgs.writeText "portal-open-file.py" ''
    import gi
    from gi.repository import Gio, GLib

    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    proxy = Gio.DBusProxy.new_sync(
        bus, Gio.DBusProxyFlags.NONE, None,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
        None,
    )
    reply = proxy.call_sync(
        "OpenFile",
        GLib.Variant("(ssa{sv})", ("", "Pick a file", {})),
        Gio.DBusCallFlags.NONE, -1, None,
    )
    print("request handle:", reply.unpack()[0], flush=True)
    GLib.MainLoop().run()
  '';

  portalOpenApp = pkgs.writeShellApplication {
    name = "portal-open-file";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      exec python3 ${portalPy}
    '';
  };

  # A window, then a modal dialog, then the dialog closed, all on timers.
  # Isolates the compositor's close handling; phrosh doesn't reliably take a
  # synthetic tap on its second request.
  windowPy = pkgs.writeText "gtk-window-demo.py" ''
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, GLib

    def on_activate(app):
        win = Gtk.ApplicationWindow(application=app, title="requesting-app")
        win.set_child(Gtk.Label(label="requesting app"))
        win.present()

        def close_window():
            # Phase D: a client-side destroy, so the client still commits and a frame
            # follows. A SIGTERM'd client never commits again.
            win.destroy()
            return False

        def close_dialog(dlg):
            dlg.destroy()
            GLib.timeout_add(4000, close_window)
            return False

        def open_dialog():
            # set_parent + the xdg-dialog hint, which State::is_dialog keys off.
            dlg = Gtk.Window(transient_for=win, modal=True, title="a-dialog")
            dlg.set_child(Gtk.Label(label="dialog"))
            dlg.present()
            GLib.timeout_add(4000, close_dialog, dlg)
            return False

        GLib.timeout_add(3000, open_dialog)

    app = Gtk.Application(application_id="org.springchick.WindowDemo")
    app.connect("activate", on_activate)
    app.run(None)
  '';

  gtkWindowApp = pkgs.writeShellApplication {
    name = "gtk-window-demo";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      export GSETTINGS_SCHEMA_DIR="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}/glib-2.0/schemas"
      export GDK_BACKEND=wayland
      export GSK_RENDERER=cairo
      exec python3 ${windowPy}
    '';
  };

  gtkChooserApp = pkgs.writeShellApplication {
    name = "gtk-chooser-demo";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      # GLib aborts if the FileChooser schema is missing, and there's no
      # wrapGAppsHook here.
      export GSETTINGS_SCHEMA_DIR="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}/glib-2.0/schemas"
      export GDK_BACKEND=wayland
      export GSK_RENDERER=cairo
      # The chooser widget is in-process regardless; this covers anything else.
      export GTK_USE_PORTAL=0
      exec python3 ${chooserPy}
    '';
  };
in
mkTest {
  name = "springchick-portal";

  packages = [
    gtkChooserApp
    gtkWindowApp
    portalOpenApp
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()

    UNIT = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def since_now():
        """A journalctl --since bound of 'right now', so each phase only ever
        matches lines its own windows produced.

        Microseconds, not whole seconds: --since is inclusive, so a
        second-granularity bound also matches everything logged earlier in the
        same second — which is long enough for a phase to be satisfied by the
        previous phase's log lines and pass without testing anything."""
        return machine.succeed("date '+%Y-%m-%d %H:%M:%S.%6N'").strip()

    def ipc(cmd):
        """Drive the compositor's synthetic-input socket as the session user.
        Coordinates are physical output px (720x1440 here).

        Note `touch`, not `tap`, for anything inside a client window: `tap` only
        runs the shell's own hit-testing (Home grid, switcher, bar gestures),
        while `touch` goes through the surface-routing path and actually
        reaches the client."""
        machine.succeed(
            "runuser -u tester -- env XDG_RUNTIME_DIR=/run/user/1000 "
            f"springchick ipc {cmd}"
        )

    def user_run(unit, cmd, extra=""):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit={unit} "
            f"--setenv=WAYLAND_DISPLAY={sock} {extra} {cmd}"
        )

    # Phase A: GTK's own chooser commits its minimum width.
    phase_a = since_now()
    user_run("gtk-chooser", "$(command -v gtk-chooser-demo)")

    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_a}' | grep -qE 'toplevel size .* oversize=true'",
        timeout=90,
    )
    machine.screenshot("01-gtk-chooser-oversize")

    # Stop it so its windows can't bleed into phase B.
    machine.succeed("systemctl --user -M tester@.host stop gtk-chooser.service || true")

    # Phase B: the same request through the portal.
    phase_b = since_now()

    # xdg-desktop-portal matches XDG_CURRENT_DESKTOP from the user manager's
    # environment, not the service's; wrong here, every call is UnknownMethod.
    machine.succeed(
        "systemctl --user -M tester@.host show-environment | "
        "grep -qx 'XDG_CURRENT_DESKTOP=springchick'"
    )

    # Holds the bus connection open (see portalPy).
    user_run("portal-open", "$(command -v portal-open-file)")

    # phrosh really running (the explicit preference beat `UseIn=phosh`). A
    # live process, not the bus name: `busctl list` shows activatable names too.
    machine.wait_until_succeeds("pgrep -f xdg-desktop-portal-phrosh", timeout=90)

    # An unmatched portals.conf fails here with UnknownMethod.
    machine.wait_until_succeeds(
        "journalctl -b _SYSTEMD_USER_UNIT=portal-open.service | grep -q 'request handle: /'",
        timeout=90,
    )

    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_b}' | grep -qE 'toplevel size .* oversize=false'",
        timeout=90,
    )
    machine.fail(f"{UNIT} --since '{phase_b}' | grep -qE 'toplevel size .* oversize=true'")
    machine.screenshot("02-portal-chooser-fits")

    # phrosh keeps its window when the peer dies, and a leftover picker would
    # satisfy phase C's `dialog=true` wait early.
    ipc("touch 100 45")
    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_b}' | grep -qE 'state changed to Home '", timeout=90
    )
    machine.succeed("systemctl --user -M tester@.host stop portal-open.service || true")

    # Phase C: a dismissed dialog returns to the app underneath.
    phase_c = since_now()
    user_run("gtk-window", "$(command -v gtk-window-demo)")

    # Pin the app's own toplevel before the dialog maps.
    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_c}' | grep -qE 'state changed to App '", timeout=90
    )
    app_state = machine.succeed(
        f"{UNIT} --since '{phase_c}' | grep -oE 'state changed to App \\{{ toplevel: [0-9]+' | tail -1"
    ).strip()
    app_toplevel = app_state.rsplit(" ", 1)[1]
    print(f"app is toplevel {app_toplevel}")

    opened = since_now()
    machine.wait_until_succeeds(
        f"{UNIT} --since '{opened}' | grep -qF 'configure toplevel dialog=true'",
        timeout=90,
    )
    dialog_state = machine.wait_until_succeeds(
        f"{UNIT} --since '{opened}' | grep -oE 'state changed to App \\{{ toplevel: [0-9]+' | tail -1",
        timeout=90,
    ).strip()
    dialog_toplevel = dialog_state.rsplit(" ", 1)[1]
    assert (
        dialog_toplevel != app_toplevel
    ), f"dialog never came forward (still toplevel {app_toplevel})"
    print(f"dialog is toplevel {dialog_toplevel}")
    machine.screenshot("03-dialog-over-app")

    # Only look at what happens after the dialog is up.
    dismissed = since_now()

    # Must land on the app's toplevel, not Home or the dialog.
    machine.wait_until_succeeds(
        f"{UNIT} --since '{dismissed}' | "
        f"grep -qE 'state changed to App \\{{ toplevel: {app_toplevel},'",
        timeout=90,
    )
    machine.fail(f"{UNIT} --since '{dismissed}' | grep -qE 'state changed to Home '")
    machine.screenshot("04-back-to-app")

    # Phase D: closing the app itself still goes Home.
    phase_d = since_now()
    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_d}' | grep -qE 'state changed to Home '", timeout=90
    )
    machine.screenshot("05-app-closed-goes-home")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
