{
  # jj-vine — stacked pull requests for jj (Jujutsu). This flake builds the
  # crate as a reproducible package so the RigelBuild fleet can consume it as a
  # `github:RigelBuild/jj-vine` flake input (pinned by flake.lock rev), instead
  # of vendoring the source as a subtree.
  #
  # Built against the fork's own dated-nightly rust-toolchain.toml via fenix (the
  # crate is edition 2024 and needs nightly rustfmt/clippy), NOT nixpkgs' stable
  # rustPlatform. The GitHub Actions CI (.github/workflows/ci.yml) runs the
  # cargo fmt/clippy/build/test gate; this build only produces the binary
  # (doCheck = false).
  description = "Stacked pull requests for jj (Jujutsu)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      fenix,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;

        # Exact toolchain from the fork's rust-toolchain.toml (dated nightly),
        # built by fenix. The sha256 pins the resolved toolchain components; bump
        # it in lockstep with the channel date in rust-toolchain.toml.
        rustToolchain = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-34aL2//JXqf+ky6X/pU7p7X8ibzel/RJmX+XqIwYDDw=";
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        jjVine = rustPlatform.buildRustPackage {
          pname = "jj-vine";
          version = "0.5.3";
          # Drop the gitignored cargo target/ and local test scaffolding from the
          # store copy; flake source-fetching already excludes VCS noise.
          src = lib.cleanSourceWith {
            src = lib.cleanSource ./.;
            filter =
              path: _type:
              let
                base = baseNameOf path;
              in
              base != "target" && base != ".env" && base != "forgejo-server";
          };
          # Committed Cargo.lock, all crates.io (no git deps → no outputHashes).
          cargoLock.lockFile = ./Cargo.lock;
          # CI (Actions) owns the test gate; this build only produces the binary.
          doCheck = false;
          meta = {
            description = "Stacked pull requests for jj (Jujutsu)";
            homepage = "https://github.com/RigelBuild/jj-vine";
            license = lib.licenses.mit;
            mainProgram = "jj-vine";
          };
        };
      in
      {
        packages.default = jjVine;
        packages.jj-vine = jjVine;

        apps.default = {
          type = "app";
          program = "${lib.getExe jjVine}";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-nextest
          ];
        };
      }
    );
}
