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

    environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
      text = cfg.config;
    };

    # DRM master + libinput come from the logind seat the greeter hands over.
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;
  };
}
