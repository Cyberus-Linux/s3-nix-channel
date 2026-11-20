{ config, lib, pkgs, ... }:
let
  cfg = config.services.s3-nix-channel;

  # The service user, local database, and database user share this name, so
  # PostgreSQL's peer authentication works without a password.
  name = "s3-nix-channel";

  createDatabaseLocally = cfg.database.url == null;
  databaseUrl =
    if createDatabaseLocally then
      "postgres://${name}@localhost/${name}?host=/run/postgresql"
    else
      cfg.database.url;

  # Run s3-nix-channel-upload with the same configuration as the service.
  adminWrapper = pkgs.writeShellApplication {
    name = "s3-nix-channel-admin";
    runtimeInputs = [ pkgs.util-linux ];
    text = ''
      if [ "$(id -u)" -ne 0 ]; then
        echo "s3-nix-channel-admin needs to run as root." >&2
        exit 1
      fi

      export DATABASE_URL=${lib.escapeShellArg databaseUrl}

      # Load the S3 credentials and, for external databases, PGPASSWORD.
      set -a
      # shellcheck source=/dev/null
      source ${lib.escapeShellArg cfg.secretsFile}
      set +a

      # Files to publish must be readable by this user.
      exec runuser -u ${name} -- ${lib.getExe' cfg.package "s3-nix-channel-upload"} "$@"
    '';
  };
in {
  options.services.s3-nix-channel = {
    enable = lib.mkEnableOption "Enables the s3-nix-channel service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.s3-nix-channel;
    };

    bucket = lib.mkOption {
      type = lib.types.str;
      description = "The name of the S3 bucket to serve.";
    };

    baseUrl = lib.mkOption {
      type = lib.types.str;
      default = "http://localhost:3000";
      description = "The base URL of the server. This is used for proper redirects.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "[::]:3000";
      description = ''
        Where to listen for connections. See
        https://www.freedesktop.org/software/systemd/man/systemd.socket.html#ListenStream=
        for more information.
      '';
    };

    secretsFile = lib.mkOption {
      type = lib.types.path;
      description = ''

        Path of a file containing S3 configuration in the format of
        EnvironmentFile as described by {manpage}`systemd.exec(5)`.

        For example:

        ```
        AWS_ACCESS_KEY_ID=...
        AWS_SECRET_ACCESS_KEY=..
        AWS_REGION=...
        AWS_ENDPOINT_URL=...
        ```

        If the database configured in
        {option}`services.s3-nix-channel.database.url` needs a password, add
        `PGPASSWORD=...` here.
      '';
    };

    database.url = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "postgres://s3-nix-channel@db.example.com/s3-nix-channel?sslmode=verify-full";
      description = ''
        URL of an external PostgreSQL database. See
        https://www.sea-ql.org/SeaORM/docs/install-and-config/connection/
        for the format.

        If this is `null`, a local PostgreSQL database is created and used via
        its Unix socket and peer authentication.

        This ends up in the world-readable Nix store, so it must not contain
        a password. Put `PGPASSWORD=...` into
        {option}`services.s3-nix-channel.secretsFile` instead.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${name} = {
      isSystemUser = true;
      group = name;
    };
    users.groups.${name} = { };

    # For managing channels. See s3-nix-channel-admin --help.
    environment.systemPackages = [ adminWrapper ];

    services.postgresql = lib.mkIf createDatabaseLocally {
      enable = true;
      ensureDatabases = [ name ];
      ensureUsers = [
        {
          inherit name;
          ensureDBOwnership = true;
        }
      ];
    };

    systemd.sockets.s3-nix-channel = {
      wantedBy = [ "sockets.target" ];
      socketConfig = {
        ListenStream = cfg.listen;
        Accept = "false";
      };
    };

    systemd.services.s3-nix-channel = {
      description = "Nix Tarball Serve";
      wantedBy = [ "multi-user.target" ];

      after = [ "network.target" ]
        ++ lib.optionals createDatabaseLocally [ "postgresql.target" ];
      requires = [ "s3-nix-channel.socket" ]
        ++ lib.optionals createDatabaseLocally [ "postgresql.target" ];

      environment.DATABASE_URL = databaseUrl;

      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";

        ExecStart = ''
          ${lib.getExe cfg.package} \
            --bucket ${cfg.bucket}  \
            --base-url ${cfg.baseUrl}
        '';

        User = name;
        Group = name;

        NoNewPrivileges = true;
        PrivateTmp = true;
        RemoveIPC = true;
        ProtectProc = "invisible";
        ProtectSystem = "strict";
        PrivateDevices = true;
        PrivateIPC = true;
        PrivateUsers = true;
        ProcSubset = "pid";
        ProtectHostname = true;
        ProtectClock = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        # TODO This can be strict on systemd 257. But 24.11 still has 256.
        ProtectControlGroups = true;
        ProtectHome = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = "AF_UNIX AF_INET AF_INET6";
        RestrictNamespaces = true;
        SystemCallFilter = "~@swap @reboot @raw-io @privileged @obsolete @mount @module @cpu-emulation @clock  @debug @resources";
        UMask = "0077";

        EnvironmentFile = cfg.secretsFile;
      };
    };
  };
}
