{
  description = "Maincopy - Git-native publishing for the open web";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      crane,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;

      projectFor =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-analyzer"
              "rust-src"
            ];
          };
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = pkgs.lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*\\.(css|html|js|json|sql|svg)$" path != null)
              || (builtins.match ".*/examples/content(/.*)?" path != null)
              || (builtins.match ".*/tests/fixtures(/.*)?" path != null)
              || (builtins.match ".*/migrations(/.*)?" path != null)
              || (builtins.match ".*/templates(/.*)?" path != null);
          };

          cargoArtifacts = craneLib.buildDepsOnly {
            inherit src;
            pname = "maincopy-dependencies";
            version = "0.1.0";
            cargoExtraArgs = "--locked";
            strictDeps = true;
          };

          maincopy = craneLib.buildPackage {
            inherit cargoArtifacts src;
            pname = "maincopy-workspace";
            version = "0.1.0";
            cargoExtraArgs = "--locked";
            strictDeps = true;
          };
        in
        {
          inherit
            cargoArtifacts
            craneLib
            maincopy
            pkgs
            rustToolchain
            src
            ;
        };
    in
    {
      packages = forAllSystems (system: {
        default = (projectFor system).maincopy;
        maincopy = (projectFor system).maincopy;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${(projectFor system).maincopy}/bin/maincopyd";
          meta.description = "Run the Maincopy server";
        };
        maincopy = {
          type = "app";
          program = "${(projectFor system).maincopy}/bin/maincopy";
          meta.description = "Operate a running Maincopy server";
        };
        maincopyd = {
          type = "app";
          program = "${(projectFor system).maincopy}/bin/maincopyd";
          meta.description = "Run the Maincopy server";
        };
      });

      checks = forAllSystems (
        system:
        let
          project = projectFor system;
        in
        {
          build = project.maincopy;

          clippy = project.craneLib.cargoClippy {
            inherit (project) cargoArtifacts src;
            cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
            strictDeps = true;
          };

          formatting = project.craneLib.cargoFmt {
            inherit (project) src;
          };

          tests = project.craneLib.cargoTest {
            inherit (project) cargoArtifacts src;
            cargoTestExtraArgs = "--all-targets --all-features";
            strictDeps = true;
          };

          nix-formatting = project.pkgs.runCommand "maincopy-nix-formatting" { } ''
            ${project.pkgs.nixfmt}/bin/nixfmt --check ${./flake.nix}
            touch "$out"
          '';

          development-gateway =
            project.pkgs.runCommand "maincopy-development-gateway"
              {
                nativeBuildInputs = [
                  project.pkgs.caddy
                  project.pkgs.curl
                  project.pkgs.jq
                  project.pkgs.openssl
                  project.pkgs.shellcheck
                ];
              }
              ''
                openssl req -x509 -newkey rsa:2048 -nodes \
                  -keyout "$TMPDIR/key.pem" \
                  -out "$TMPDIR/certificate.pem" \
                  -subj "/CN=admin.localhost" \
                  -addext "subjectAltName=DNS:admin.localhost,DNS:maincopy.localhost" \
                  -days 1 >/dev/null 2>&1
                export MAINCOPY_DEV_TLS_CERTIFICATE="$TMPDIR/certificate.pem"
                export MAINCOPY_DEV_TLS_PRIVATE_KEY="$TMPDIR/key.pem"
                caddy validate --config ${./dev/Caddyfile} --adapter caddyfile
                shellcheck \
                  ${./scripts/dev-alpha.sh} \
                  ${./scripts/dev-gateway.sh} \
                  ${./scripts/dev-maincopy.sh} \
                  ${./scripts/test-dev-gateway.sh}
                ${project.pkgs.bash}/bin/bash \
                  ${./scripts/test-dev-gateway.sh} ${./dev/Caddyfile}
                touch "$out"
              '';
        }
      );

      formatter = forAllSystems (system: (projectFor system).pkgs.nixfmt-tree);

      devShells = forAllSystems (
        system:
        let
          project = projectFor system;
        in
        {
          default = project.pkgs.mkShell {
            inputsFrom = [ project.maincopy ];
            packages = [
              project.pkgs.caddy
              project.pkgs.curl
              project.pkgs.jq
              project.pkgs.litestream
              project.pkgs.mkcert
              project.pkgs.nixfmt-tree
              project.pkgs.nssTools
              project.pkgs.shellcheck
              project.pkgs.sqlite
              project.pkgs.util-linux
              project.rustToolchain
            ];

            RUST_BACKTRACE = "1";
          };
        }
      );
    };
}
