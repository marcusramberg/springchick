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
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Puts share/wayland-sessions/springchick.desktop into the system profile so
    # greeters (greetd's regreet/gtkgreet, GDM, …) list springchick as a session.
    services.displayManager.sessionPackages = [ cfg.package ];

    # DRM master + libinput come from the logind seat the greeter hands over.
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;
  };
}
