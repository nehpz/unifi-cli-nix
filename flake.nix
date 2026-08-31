{
  description = "Nix packaging for unifi-cli, a CLI for UniFi Network controllers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      # x86_64-darwin is absent: Nixpkgs 26.11 dropped it; evaluating it throws,
      # which would break `nix flake show` and FlakeHub's include-output-paths evaluation.
      # aarch64-linux is absent deliberately: it builds, but no consumer of this
      # tool targets it, and declaring a platform CI does not build is an untested claim.
      # overlays.default is system-agnostic and meta.platforms is unix, so any unix consumer (including aarch64-linux) can still build via the overlay.
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;

      unifi-cli =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "unifi-cli";
          version = (lib.importTOML ./Cargo.toml).package.version;
          # tests/no_real_network_data.rs scans README.md and CHANGELOG.md, and silently skips them if absent.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./src
              ./tests
              ./Cargo.toml
              ./Cargo.lock
              ./README.md
              ./CHANGELOG.md
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "CLI for UniFi Network controller";
            homepage = "https://github.com/nehpz/unifi-cli-nix";
            license = lib.licenses.mit;
            mainProgram = "unifi";
            # Intentionally broader than `systems`: keeps the overlay usable on
            # systems this flake does not enumerate.
            platforms = lib.platforms.unix;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          pkg = unifi-cli pkgs;
        in
        {
          unifi-cli = pkg;
          default = pkg;
        }
      );

      overlays.default = final: prev: {
        unifi-cli = unifi-cli final;
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
            ];
          };
        }
      );

      checks = forAllSystems (system: {
        unifi-cli = self.packages.${system}.unifi-cli;
      });

      # pkgs.nixfmt is the RFC-style formatter; nixfmt-rfc-style is now an alias.
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
