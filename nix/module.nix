{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.springchick;
in
{
  options.programs.springchick = {
    enable = lib.mkEnableOption "springchick, a Springboard-style Wayland compositor";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.springchick;
      defaultText = lib.literalMD "`packages.<system>.springchick` from the springchick flake";
      description = "The springchick package to install.";
    };

    # Named `config`, not `keybindings`: config.toml is a general file (a
    # [keybinds] table today, more sections later), and `cfg.config` here is
    # unrelated to this module's own `config = lib.mkIf cfg.enable { ... }`
    # output attribute below — same name, two different things.
    config = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      description = ''
        Raw TOML written to /etc/springchick/config.toml. See
        crates/sc-keys/src/config.rs for the [keybinds] table schema.
        Null (default) leaves the compositor's built-in defaults in
        place and does not touch /etc.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Puts share/wayland-sessions/springchick.desktop into the system profile so
    # greeters (greetd's regreet/gtkgreet, GDM, …) list springchick as a session.
    services.displayManager.sessionPackages = [ cfg.package ];

    # The compositor runs as a Type=notify user service rather than being exec'd
    # straight from the greeter. This is the niri model and the only correct way
    # to satisfy the graphical-session.target contract: the service
    # BindsTo=+Before= that target, so when the compositor sends sd_notify READY
    # the target is pulled active. graphical-session.target is RefuseManualStart,
    # so nothing may start it by hand — the binding is the sanctioned path. Once
    # active, xdg-desktop-portal-*, the OSK and anything else gating on a live
    # graphical session can finally start. nix/springchick-session drives this:
    # import-environment → start this service → force springchick-shutdown.target.
    systemd.user.services.springchick = {
      description = "springchick Wayland compositor";
      documentation = [ "https://github.com/marcusramberg/springchick" ];
      # Deliberately not WantedBy any target: it is started on demand by
      # springchick-session, never pulled up automatically at login.
      bindsTo = [ "graphical-session.target" ];
      before = [
        "graphical-session.target"
        "xdg-desktop-autostart.target"
      ];
      after = [ "graphical-session-pre.target" ];
      wants = [
        "graphical-session-pre.target"
        "xdg-desktop-autostart.target"
      ];
      # NixOS defaults this to true, which pins a stripped Environment=PATH=
      # (per-unit, highest precedence) onto the service and shadows the full
      # login PATH that springchick-session imports into the user manager.
      # With it off the compositor — and every app/shortcut it spawns —
      # inherits that imported login PATH. Same fix as niri.service upstream.
      enableDefaultPath = false;
      environment = {
        # Device backend (DRM/libseat), previously set on the session wrapper.
        SPRINGCHICK_BACKEND = "drm";
        XDG_CURRENT_DESKTOP = "springchick";
        XDG_SESSION_TYPE = "wayland";
      };
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        Slice = "session.slice";
        ExecStart = "${cfg.package}/bin/springchick";
        # If the compositor dies the session is over; do not respawn it.
        Restart = "no";
        # Compositor holds the DRM master + input; give it room to shut down.
        TimeoutStopSec = "10s";
      };
    };

    # Force-teardown target. springchick-session starts this after the
    # compositor exits; conflicting with graphical-session.target(-pre) stops
    # the whole session tree irreversibly, mirroring niri-shutdown.target.
    systemd.user.targets.springchick-shutdown = {
      description = "Shutdown running springchick session";
      unitConfig = {
        DefaultDependencies = false;
        StopWhenUnneeded = true;
      };
      conflicts = [
        "graphical-session.target"
        "graphical-session-pre.target"
      ];
      after = [
        "graphical-session.target"
        "graphical-session-pre.target"
      ];
    };

    environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
      text = cfg.config;
    };

    # Reference config with every option at its default, for users to copy to
    # config.toml and edit. Kept as .example so it never shadows the built-in
    # defaults or a user's own /etc/springchick/config.toml.
    environment.etc."springchick/config.toml.example".source =
      "${cfg.package}/share/springchick/config.example.toml";

    # DRM master + libinput come from the logind seat the greeter hands over.
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;

    # xdg-desktop-portal: needed for apps like Fractal (Matrix secrets portal),
    # file pickers, screenshots, etc. `config.springchick` writes
    # /etc/xdg/xdg-desktop-portal/springchick-portals.conf, matched by
    # XDG_CURRENT_DESKTOP=springchick set in the service environment.
    # Mirrors niri.nix upstream.
    xdg.portal = {
      enable = true;
      extraPortals = [ pkgs.xdg-desktop-portal-gnome ];
      config.springchick = {
        default = [ "gnome" "gtk" ];
        # Secret portal only works with gnome backend (delegates to gnome-keyring).
        "org.freedesktop.impl.portal.Secret" = "gnome-keyring";
      };
    };

    # Required for gnome-keyring Secret portal backend.
    services.gnome.gnome-keyring.enable = lib.mkDefault true;
  };
}
