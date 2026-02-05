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
        local-packages-overlay = final: _prev: import ./nix/packages { callPackage = final.callPackage; };
        overlays = [
          (import rust-overlay)
          local-packages-overlay
        ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        localPackages = import ./nix/packages { callPackage = pkgs.callPackage; };
      in
      {
        packages = localPackages;

        devShells.default = pkgs.mkShell {
          buildInputs =
            with pkgs;
            [
              (rust-bin.stable.latest.default.override {
                extensions = [
                  "rust-src"
                  "llvm-tools-preview"
                ];
              })
              capnproto
              capnproto-rust

              cargo-llvm-cov
              cargo-flamegraph
              cargo-bloat
              cargo-llvm-lines
            ]
            ++ lib.optionals stdenv.isLinux [
              heaptrack
            ]
            ++ lib.optionals stdenv.isDarwin [
              cargo-instruments
            ];
        };
      }
    )
    // {
      inputs = { inherit nixpkgs rust-overlay; };
    };
}
