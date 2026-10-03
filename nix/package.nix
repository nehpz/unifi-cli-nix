{ lib, rustPlatform }:

let
  manifest = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "unifi-cli";
  version = manifest.package.version;
  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  doCheck = true;
  # Integration tests start HTTP mock servers on loopback.
  __darwinAllowLocalNetworking = true;

  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    for command in unifi unifi-cli; do
      test "$("$out/bin/$command" --version)" = "unifi ${manifest.package.version}"
      "$out/bin/$command" --help > /dev/null
    done
    runHook postInstallCheck
  '';

  meta = {
    inherit (manifest.package) description homepage;
    license = lib.licenses.mit;
    mainProgram = "unifi";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];
  };
}
