{ config, lib, pkgs, ogygia-irisd, ... }:

let
  cfg = config.ogygia.irisd;

  tomlFormat = pkgs.formats.toml { };

  # Use the first listen address for the local Nix substituter URL
  substituterUrl = "http://${builtins.head cfg.settings.server.listen}";

  configFile = tomlFormat.generate "ogygia-irisd.toml" cfg.settings;
in
{
  options.ogygia.irisd = {
    enable = lib.mkEnableOption "ogygia-irisd peer-to-peer Nix binary cache";

    package = lib.mkOption {
      type = lib.types.package;
      default = ogygia-irisd;
      description = "The ogygia-irisd package to use.";
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = tomlFormat.type;
        options = {
          server.listen = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default =
              [ "127.0.0.1:35742" ]
              ++ lib.optionals (config.ogygia.nebula.ipv4 != null) [
                "${config.ogygia.nebula.ipv4}:35742"
              ];
            defaultText = lib.literalExpression ''[ "127.0.0.1:35742" ] ++ optional nebula.ipv4'';
            description = ''
              HTTP listen addresses for the binary cache.
              Defaults to localhost and, when Nebula is configured, the
              Nebula IPv4 address.
            '';
          };
        };
      };
      default = { };
      description = ''
        Freeform configuration for ogygia-irisd, serialized directly to
        TOML. Keys map 1:1 to the application config schema. Any key not
        set here uses the application's compiled-in default.

        Commonly used keys:

        - `bloom.false_positive_rate` (float) — target bloom filter
          false-positive rate. Lower values use more memory but reduce
          unnecessary NAR fetches from peers.

        - `bloom.rebuild_threshold` (float) — ratio of deletions to
          elements at which the bloom filter is rebuilt from a fresh
          /nix/store scan. Lower values keep the filter accurate but
          trigger more rebuilds.

        - `bloom.peer_bloom_ttl_secs` (int) — how long a peer's bloom
          filter is cached before re-fetching.

        - `bloom.peer_bloom_max_age_secs` (int) — maximum age for a
          peer's bloom filter before it is discarded. Between
          `peer_bloom_ttl_secs` and this value, stale blooms are served
          while a background re-fetch is attempted. Defaults to
          2× `peer_bloom_ttl_secs`.

        - `peers.urls` (list of string) — peer irisd HTTP URLs for
          bloom-based store path lookup.

        - `trust.trusted_keys` (list of string) — Nix public signing
          keys trusted when fetching NARs from peers.
          Format: "name:base64-public-key".

        - `cache.dir` (path) — directory for cached zstd-compressed
          NAR files. Default: /var/cache/ogygia-irisd/nar.

        - `cache.time_to_idle_secs` (int) — seconds before idle
          cached NARs are evicted. Default: 3600. Set to 0 to
          disable TTI eviction.

        - `cache.max_size_bytes` (int) — maximum total cache size
          in bytes. Default: 10737418240 (10 GiB). Set to 0 for
          unlimited.
      '';
    };

    configureNixDaemon = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Configure the local Nix daemon to use irisd as a preferred substituter.

        When enabled, irisd is added as a preferred substituter, allowing the
        local machine to fetch store paths from peers via bloom filter lookup.
        Other substituters (like cache.nixos.org) remain configured.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.settings.server.listen != [ ];
        message = "ogygia.irisd.settings.server.listen must not be empty.";
      }
    ];

    systemd.services.ogygia-irisd = {
      description = "Ogygia Peer-to-Peer Nix Binary Cache";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "nix-daemon.socket" ];
      requires = [ "nix-daemon.socket" ];

      path = [ config.nix.package ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/ogygia-irisd --config ${configFile}";
        Restart = "always";
        RestartSec = "60s";
        DynamicUser = true;
        RuntimeDirectory = "ogygia-irisd";
        CacheDirectory = "ogygia-irisd";

        # Security hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;

        # Read access to /nix/store for on-demand NAR generation
        # and to nix daemon socket for path-info queries
        ReadOnlyPaths = [ "/nix/store" "/nix/var/nix/daemon-socket" ];
      };

      environment = {
        RUST_LOG = lib.mkDefault "info";
        # Enable nix-command feature for nix path-info queries
        NIX_CONFIG = "experimental-features = nix-command";
      };
    };

    # Trust the same keys as the Nix daemon by default
    ogygia.irisd.settings.trust.trusted_keys = lib.mkDefault config.nix.settings.trusted-public-keys;

    # Configure Nix daemon to use irisd as a preferred substituter
    nix.settings = lib.mkIf cfg.configureNixDaemon {
      substituters = lib.mkBefore [ substituterUrl ];
    };
  };
}
