{
  lib,
  fetchCrate,
  rustPlatform,
  openssl,
  pkg-config,
}:
rustPlatform.buildRustPackage rec {
  pname = "cargo-instruments";
  version = "0.4.13";

  src = fetchCrate {
    inherit pname version;
    sha256 = "sha256-rK++Z3Ni4yfkb36auyWJ9Eiqi2ATeEyQ6J4synRTpbM=";
  };

  cargoHash = "sha256-hRpWBt00MHMBZCHAsbFU0rwpsoavv6PUNj6owFHRNEw=";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  # Only build on macOS since it requires Instruments.app
  meta = {
    platforms = lib.platforms.darwin;
  };
}
