{
  description = "Nix package and development environment for unifi-cli";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        default = pkgs.callPackage ./nix/package.nix { };
        "unifi-cli" = default;
      });

      overlays.default = final: _prev: {
        "unifi-cli" = final.callPackage ./nix/package.nix { };
      };

      checks = forAllSystems (pkgs: {
        overlay = pkgs.callPackage ./nix/overlay-check.nix {
          overlay = self.overlays.default;
          packageName = "unifi-cli";
        };
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-nextest
            nixfmt
          ];
          env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
