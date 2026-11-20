{ self, system }:
let
  pkgs = self.inputs.nixpkgs.legacyPackages.${system};

  accessKey = "GKaaaaaaaaaaaaaaaaaaaaaaaa";
  secretKey = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  region = "eu-central-1";
  bucket = "channels";

  secretsFile = pkgs.writeText "fake-secrets" ''
    AWS_ACCESS_KEY_ID=${accessKey}
    AWS_SECRET_ACCESS_KEY=${secretKey}
    AWS_REGION=${region}
    AWS_ENDPOINT_URL=http://s3:9000
  '';

  tarball =
    pkgs.runCommand "tarball-1234.tar.xz"
      {
        nativeBuildInputs = [ pkgs.libarchive ];
      }
      ''
        mkdir $out

        mkdir foo
        touch foo/hello

        # The original tarball.
        tar -cJf $out/tarball-1234.tar.xz foo

        # Create an updated tarball.
        touch foo/world
        tar -cJf $out/tarball-1235.tar.xz foo

        # Create yet another updated tarball.
        touch foo/again
        tar -cJf $out/tarball-1236.tar.xz foo
      '';

  isoImage =
    pkgs.runCommand "media-1234.iso"
      {
        nativeBuildInputs = [ pkgs.cdrkit ];
      }
      ''
        mkdir root
        mkdir $out

        genisoimage -o $out/media-1234.iso root/

        touch root/updated
        genisoimage -o $out/media-1235.iso root/
      '';

  tarballServeCommon =
    { config, pkgs, ... }:
    {
      nix.extraOptions = ''
        experimental-features = nix-command flakes
      '';

      environment.systemPackages = with pkgs; [
        git
        jq
      ];

      services.s3-nix-channel = {
        enable = true;
        secretsFile = "${secretsFile}";
        listen = "0.0.0.0:80";
        baseUrl = "http://localhost";

        inherit bucket;
      };
    };
in
{
  canServeFiles = pkgs.testers.nixosTest {
    name = "s3-nix-channel";

    nodes = {
      s3 = {
        services.garage = {
          enable = true;
          package = pkgs.garage_2;

          settings = {
            replication_factor = 1;
            consistent_mode = "consistent";

            rpc_bind_addr = "[::]:3901";
            rpc_public_addr = "[::]:3901";
            rpc_secret = "5c1915fa04d0b6739675c61bf5907eb0fe3d9c69850c83820f51b4d25d13868c";

            s3_api = {
              s3_region = region;
              api_bind_addr = "[::]:9000";
              root_domain = ".s3.garage";
            };

          };
        };

        environment.systemPackages = [
          pkgs.minio-client
        ];

        networking.firewall.enable = false;
      };

      servePublic = {
        imports = [
          self.nixosModules.default
          tarballServeCommon
        ];
      };
    };

    testScript = ''
      def upload(args):
          """The command line to manage channels with the given arguments."""
          return f"s3-nix-channel-admin {args}"

      s3.start()
      s3.wait_for_unit("garage.service")
      s3.wait_for_open_port(3901)

      # Create cluster
      node_id = s3.succeed("garage status | tail -n1 | cut -d' ' -f1")
      s3.succeed(f"garage layout assign -z dc1 -c 128M {node_id}")
      s3.succeed("garage layout apply --version 1")

      # Create bucket
      s3.succeed("garage bucket create ${bucket}")

      # Create access keys
      s3.succeed("garage key import ${accessKey} ${secretKey} --yes")
      s3.succeed("garage bucket allow --read --write --owner ${bucket} --key ${accessKey}")

      s3.wait_for_open_port(9000)

      ## Prepare the bucket of tarballs with configuration.

      # Garage sometimes takes a second to come up.
      s3.wait_until_succeeds("mc alias set local http://localhost:9000 ${accessKey} ${secretKey}")

      ## Start our server.
      servePublic.start()

      # It should come up with an empty bucket.
      servePublic.wait_for_unit("s3-nix-channel.service")

      # Populate the configuration
      servePublic.succeed("mkdir content")
      servePublic.copy_from_host("${tarball}/tarball-1234.tar.xz", "content/tarball-1234.tar.xz");
      servePublic.copy_from_host("${tarball}/tarball-1235.tar.xz", "content/tarball-1235.tar.xz");
      servePublic.copy_from_host("${tarball}/tarball-1236.tar.xz", "content/tarball-1236.tar.xz");
      servePublic.copy_from_host("${isoImage}/media-1234.iso", "content/media-1234.iso")
      servePublic.copy_from_host("${isoImage}/media-1235.iso", "content/media-1235.iso")

      servePublic.succeed(upload("add-channel thechannel-24.05.tar.xz"))
      servePublic.succeed(upload("add-channel install-24.05.iso"))

      servePublic.succeed(upload("publish ${bucket} thechannel-24.05.tar.xz content/tarball-1234.tar.xz"))
      servePublic.succeed(upload("publish ${bucket} install-24.05.iso content/media-1234.iso"))

      servePublic.succeed("curl -vL http://localhost/channel/thechannel-24.05.tar.xz > latest.tar.xz")
      servePublic.succeed("curl -vL http://localhost/permanent/tarball-1234.tar.xz > permanent.tar.xz")

      servePublic.succeed("cmp content/tarball-1234.tar.xz latest.tar.xz")
      servePublic.succeed("cmp content/tarball-1234.tar.xz permanent.tar.xz")

      # Channels redirect to our own permanent URL and never to (expiring)
      # pre-signed S3 URLs. The permanent URL is served directly.
      assert "http://localhost/permanent/tarball-1234.tar.xz" == servePublic.succeed("curl -s -o /dev/null -w '%{redirect_url}' http://localhost/channel/thechannel-24.05.tar.xz")
      assert "404" == servePublic.succeed("curl -s -o /dev/null -w '%{http_code}' http://localhost/permanent/does-not-exist.tar.xz")

      # Conditional requests are passed on to S3, so clients with a current
      # cached copy get a 304 instead of the whole file.
      permanent = "http://localhost/permanent/tarball-1234.tar.xz"
      etag = servePublic.succeed(f"curl -s -o /dev/null -w '%header{{etag}}' {permanent}")
      last_modified = servePublic.succeed(f"curl -s -o /dev/null -w '%header{{last-modified}}' {permanent}")
      assert etag != "", "No ETag header"
      assert last_modified != "", "No Last-Modified header"

      assert "304" == servePublic.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -H 'If-None-Match: {etag}' {permanent}")
      assert "304" == servePublic.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' --head -H 'If-None-Match: {etag}' {permanent}")
      assert etag == servePublic.succeed(f"curl -s -o /dev/null -w '%header{{etag}}' -H 'If-None-Match: {etag}' {permanent}")
      assert "304" == servePublic.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -H 'If-Modified-Since: {last_modified}' {permanent}")
      assert "200" == servePublic.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -H 'If-None-Match: \"stale\"' {permanent}")

      # Now with HEAD requests
      assert "200" == servePublic.succeed("curl -s -o /dev/null -w '%{http_code}' --head -vL http://localhost/channel/thechannel-24.05.tar.xz")
      assert "200" == servePublic.succeed("curl -s -o /dev/null -w '%{http_code}' --head -vL http://localhost/permanent/tarball-1234.tar.xz")

      # Now the same with custom file endings
      servePublic.succeed("curl -vL http://localhost/channel/install-24.05.iso > latest.iso")
      servePublic.succeed("curl -vL http://localhost/permanent/media-1234.iso > permanent.iso")

      servePublic.succeed("cmp content/media-1234.iso latest.iso")
      servePublic.succeed("cmp content/media-1234.iso permanent.iso")

      # Add an alias
      servePublic.succeed(upload("add-alias thechannel-24.05.tar.xz foobar-24.05.tar.xz"))

      # Fail to add names that are already in use, such as other channel names.
      # --force skips the extension check, so only the duplicate name can fail.
      servePublic.fail(upload("add-alias --force thechannel-24.05.tar.xz install-24.05.iso"))

      # Can fetch via alias
      servePublic.succeed("curl -vL http://localhost/channel/foobar-24.05.tar.xz > alias.tar.xz")
      servePublic.succeed("cmp content/tarball-1234.tar.xz alias.tar.xz")

      # Can publish via alias
      servePublic.succeed(upload("publish ${bucket} foobar-24.05.tar.xz content/tarball-1235.tar.xz"))
      servePublic.succeed("curl -vL http://localhost/channel/foobar-24.05.tar.xz > alias-new.tar.xz")
      servePublic.succeed("cmp content/tarball-1235.tar.xz alias-new.tar.xz")
      servePublic.succeed("curl -vL http://localhost/channel/thechannel-24.05.tar.xz > alias-new2.tar.xz")
      servePublic.succeed("cmp content/tarball-1235.tar.xz alias-new2.tar.xz")

      ## Check whether the channel works as flake input.
      servePublic.succeed("mkdir -p flake")
      servePublic.copy_from_host("${./test-flake.nix}", "flake/flake.nix")
      servePublic.succeed("cd flake ; git init ; git add flake.nix ; nix flake lock")

      # Check whether the lock file records the right permanent URL.
      assert "http://localhost/permanent/tarball-1235.tar.xz\n" == servePublic.succeed("jq -r .nodes.thechannel.locked.url flake/flake.lock")

      # Check whether we can update the tarball.
      print(servePublic.succeed(upload("publish ${bucket} thechannel-24.05.tar.xz content/tarball-1236.tar.xz")))

      # Check whether we can update files with different extensions as well.
      print(servePublic.succeed(upload("publish ${bucket} install-24.05.iso content/media-1235.iso")))

      # Check adding a channel works
      servePublic.succeed(upload("add-channel new-24.05.txt"))

      # Fail to upload duplicate files
      servePublic.fail(upload("publish ${bucket} install-24.05.iso content/media-1235.iso"))

      # Fail to add duplicate channel
      servePublic.fail(upload("add-channel install-24.05.iso"))

      # Check whether the flake updates to the new version
      servePublic.succeed("cd flake ; nix flake update")
      assert "http://localhost/permanent/tarball-1236.tar.xz\n" == servePublic.succeed("jq -r .nodes.thechannel.locked.url flake/flake.lock")
    '';
  };
}
