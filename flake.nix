{
  description = "Build a cargo project without extra checks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    devenv.url = "github:cachix/devenv";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      devenv,
      crane,
      flake-utils,
      ...
    }:
    {
      herculesCI.ciSystems = [
        "x86_64-linux"
      ];

      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        {
          imports = [
            ./nix/module.nix
          ];

          nixpkgs.overlays = [
            (final: prev: {
              s3-nix-channel = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            })
          ];
        };

    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        craneLib = crane.mkLib pkgs;

        # Common arguments can be set here to avoid repeating them later
        # Note: changes here will rebuild all dependency crates
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          buildInputs = [
            # Add additional build inputs here
          ];
        };

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        s3-nix-channel = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;

            # Additional environment variables or build phases/hooks can be set
            # here *without* rebuilding all dependency crates
            # MY_CUSTOM_VAR = "some value";

            meta = {
              mainProgram = "s3-nix-channel";
            };
          }
        );
      in
      {
        checks = {
          inherit s3-nix-channel;

          # Run clippy (and deny all warnings) on the crate source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
          s3-nix-channel-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
        }
        // import ./nix/tests.nix { inherit self system; };

        packages.default = s3-nix-channel;

        apps.default = flake-utils.lib.mkApp {
          drv = s3-nix-channel;
        };

        devShells.default = devenv.lib.mkShell {
          inherit inputs pkgs;

          modules = [
            (
              {
                pkgs,
                lib,
                config,
                ...
              }:
              {
                languages.rust.enable = true;

                env = {
                  AWS_ACCESS_KEY_ID = "local-test-access-key";
                  AWS_SECRET_ACCESS_KEY = "local-test-secret-key-123456";
                  AWS_DEFAULT_REGION = config.services.garage.region;
                  AWS_ENDPOINT_URL = "http://${config.services.garage.s3Address}";
                  AWS_ALLOW_HTTP = "true";
                };

                # devenv.sh/services/
                services = {
                  garage.enable = true;
                  garage.buckets = [ "dev-bucket" ];

                  garage.afterStart = ''
                    garage key import --yes -n app-key $AWS_ACCESS_KEY_ID $AWS_SECRET_ACCESS_KEY || true

                    # 3. Grant the hardcoded key permissions to the bucket
                    garage bucket allow --read --write --owner dev-bucket --key app-key
                  '';

                  postgres.enable = true;
                  postgres.listen_addresses = "localhost"; # The default port is 5432.
                  postgres.initialDatabases = [
                    {
                      name = "app";
                      user = "app";
                      pass = "secret";
                    }
                  ];

                  postgres.initialScript = ''
                    CREATE ROLE postgres SUPERUSER;
                  '';
                };
              }
            )
          ];
        };
      }
    );
}
