{
  description = "Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      flake-utils,
      nixpkgs,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "llvm-tools-preview"
          ];
          targets = [ "thumbv7em-none-eabihf" ];
        };
        mkRustCheck =
          name: command:
          pkgs.rustPlatform.buildRustPackage {
            pname = name;
            version = "0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            nativeBuildInputs = [
              pkgs.capnproto
              pkgs.capnproto-rust
            ];
            buildPhase = ''
              runHook preBuild
              runHook postBuild
            '';
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              export CARGO_TARGET_DIR="$TMPDIR/target"
              export CARGO_BUILD_TARGET="${pkgs.stdenv.hostPlatform.rust.rustcTarget}"
              ${command}
              runHook postCheck
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p "$out"
              runHook postInstall
            '';
          };
        commonChecks = {
          thincan-all-features =
            mkRustCheck "thincan-all-features-check"
              "cargo test -p thincan --all-features";
          thincan-file-transfer-all-features =
            mkRustCheck "thincan-file-transfer-all-features-check"
              "cargo test -p thincan-file-transfer --all-features";
          embedded-can-unix-socket-integration =
            mkRustCheck "embedded-can-unix-socket-integration-check"
              "cargo test -p embedded-can-unix-socket --test bus -- --nocapture";
          thincan-file-transfer-uds-e2e =
            mkRustCheck "thincan-file-transfer-uds-e2e-check"
              "cargo test -p thincan-example --test file_transfer_uds_e2e --features uds -- --nocapture";
          can-iso-tp-defmt-compile =
            mkRustCheck "can-iso-tp-defmt-compile-check"
              "cargo check -p can-iso-tp --no-default-features --features \"alloc uds isotp-interface defmt\"";
        };
        linuxChecks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          workspace-default =
            mkRustCheck "workspace-default-check"
              "cargo test --workspace";
          can-iso-tp-all-non-defmt =
            mkRustCheck "can-iso-tp-all-non-defmt-check"
              "cargo test -p can-iso-tp --no-default-features --features \"std alloc uds isotp-interface\"";
          embedded-can-socketcan-all-features =
            mkRustCheck "embedded-can-socketcan-all-features-check"
              "cargo test -p embedded-can-socketcan --all-features";
          linux-socketcan-iso-tp-all-features =
            mkRustCheck "linux-socketcan-iso-tp-all-features-check"
              "cargo test -p linux-socketcan-iso-tp --all-features";
          can-iso-tp-kernel-socketcan-integration =
            mkRustCheck "can-iso-tp-kernel-socketcan-integration-check"
              "cargo test -p can-iso-tp --test kernel_socketcan -- --nocapture";
          thincan-example-socketcan-build =
            mkRustCheck "thincan-example-socketcan-build-check"
              "cargo test -p thincan-example --no-run --features \"socketcan socketcan-isotp\"";
        };
        darwinChecks = pkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
          workspace-no-socketcan =
            mkRustCheck "workspace-no-socketcan-check"
              "cargo test --workspace --exclude embedded-can-socketcan --exclude linux-socketcan-iso-tp --exclude thincan-example";
        };
        localPackages = import ./nix/packages {
          callPackage = pkgs.callPackage;
          lib = pkgs.lib;
          stdenv = pkgs.stdenv;
        };
      in
      {
        packages = localPackages;

        devShells.default = pkgs.mkShell {
          buildInputs =
            [
              toolchain
              pkgs.capnproto
              pkgs.capnproto-rust

              pkgs.cargo-llvm-cov
              pkgs.cargo-flamegraph
              pkgs.cargo-bloat
              pkgs.cargo-llvm-lines
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.heaptrack
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              localPackages.cargo-instruments
            ];
        };

        checks = commonChecks // linuxChecks // darwinChecks;
      }
    )
    ;
}
