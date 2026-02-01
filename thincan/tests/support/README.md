The Cap'n Proto support tests keep a committed generated file so that building `thincan`
does not require the `capnp` schema compiler.

- Source schema: `tests/support/schema/person.capnp`
- Generator annotations: `tests/support/schema/rust.capnp`
- Generated Rust (checked in): `tests/support/person_capnp.rs`

To regenerate `person_capnp.rs` after updating Cap'n Proto tooling versions:

`cargo run -p thincan --features capnp-gen --bin gen_test_capnp`
