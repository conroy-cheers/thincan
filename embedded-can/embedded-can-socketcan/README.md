# embedded-can-socketcan

Linux SocketCAN adapters that implement the `embedded-can-interface` traits.

This crate provides small wrapper types around the [`socketcan`](https://crates.io/crates/socketcan)
crate's sockets, so higher-level protocol crates (e.g. ISO-TP) can be written against the
`embedded-can-interface` traits.

## Platform support

- Linux: full support (SocketCAN).
- Non-Linux: the crate builds, but constructors return an `UnsupportedPlatformError`.

