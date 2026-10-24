# Evaluates ogygia-managed Nebula for a device with no system configuration,
# such as a Mac running the Ogygia app. The result is an `ogygiaConfigurations`
# entry: `ogygia nebula rekey` signs its cert like any NixOS host's, and the
# device evaluates `config.ogygia.nebula.settings` for its Nebula config, and
# `topology` to name the hosts it shows.
#
# `name` is the device's identity in the mesh: its attribute name, its key in
# `topology.hosts` and the name in its cert. `modules` set `ogygia.nebula.*`
# as a NixOS host would, typically the fleet's shared topology plus the
# device's own pubKey and groups.
{ name, lib, modules ? [ ] }:

lib.evalModules {
  modules = [
    ./common.nix
    {
      ogygia.nebula = {
        name = lib.mkDefault name;
        # A device is defined only to join the mesh.
        enable = lib.mkDefault true;
      };
    }
  ] ++ modules;
}
