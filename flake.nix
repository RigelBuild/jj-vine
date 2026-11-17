{
  description = "jj-vine, installed from the RigelBuild/jj-vine release binaries";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";

  outputs =
    { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;

      version = "0.5.4-rigel.3";

      # One upstream release asset per system. Each sha256 is bare hex, copied
      # from the asset as published on the v${version} GitHub release.
      artifacts = {
        aarch64-darwin = {
          target = "aarch64-apple-darwin";
          sha256 = "f33e3d2869575e8ea769a9a374a95b0ef87c04850d204ac8a08b2aa13106cbb5";
        };
        aarch64-linux = {
          target = "aarch64-unknown-linux-musl";
          sha256 = "9f7e4e6947817013034d7d3cc8ce75b361d87bc6392b59ad7a421576637215a0";
        };
        x86_64-linux = {
          target = "x86_64-unknown-linux-musl";
          sha256 = "63264e343ea405cf6979aa159079ce057fb1e3079c87d7b25f24f5af12c8b5f0";
        };
      };

      systems = builtins.attrNames artifacts;
      forAllSystems = lib.genAttrs systems;

      # NOT rustPlatform.buildRustPackage: install the binary upstream built, so
      # no host compiles jj-vine at switch time and no Rust inputs enter the
      # closure. The release archive holds `jj-vine` at its root, so point the
      # build at the unpack dir itself.
      mkJjVine =
        pkgs: artifact:
        pkgs.stdenvNoCC.mkDerivation {
          pname = "jj-vine";
          inherit version;
          src = pkgs.fetchurl {
            url = "https://github.com/RigelBuild/jj-vine/releases/download/v${version}/jj-vine-${version}-${artifact.target}.tar.gz";
            inherit (artifact) sha256;
          };
          sourceRoot = ".";
          # Do NOT add strip: it would rewrite the upstream binary, and the
          # darwin arm64 build needs its ad-hoc code signature intact to exec.
          installPhase = ''
            runHook preInstall
            install -Dm755 jj-vine "$out/bin/jj-vine"
            runHook postInstall
          '';
          meta = {
            description = "Stacked pull requests for jj (jujutsu)";
            homepage = "https://codeberg.org/abrenneke/jj-vine";
            license = pkgs.lib.licenses.mit;
            mainProgram = "jj-vine";
            platforms = systems;
          };
        };

      packages = forAllSystems (system: {
        jj-vine = mkJjVine nixpkgs.legacyPackages.${system} artifacts.${system};
      });
    in
    {
      inherit packages;

      # Evaluates each package's derivation without realising it. The drv path
      # is written out as text with its string context dropped, so the check
      # neither depends on nor builds the package.
      checks = forAllSystems (system: {
        jj-vine = nixpkgs.legacyPackages.${system}.writeText "jj-vine-drv-path" (
          builtins.unsafeDiscardStringContext packages.${system}.jj-vine.drvPath
        );
      });
    };
}
