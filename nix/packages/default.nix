{ callPackage, lib, stdenv }:
lib.optionalAttrs stdenv.isDarwin {
  cargo-instruments = callPackage ./cargo-instruments { };
}
