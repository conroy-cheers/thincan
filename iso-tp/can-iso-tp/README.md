# can-iso-tp

Lightweight ISO-TP (ISO 15765-2) transport for CAN.

This crate provides blocking/polling and async ISO-TP nodes built on top of the
`embedded-can-interface` traits, plus supporting types for addressing, PDUs, and Tx/Rx state.

## no_std / no-alloc

Receive-side reassembly requires a buffer. In `no_std`/`no alloc` environments, you typically
provide a fixed slice (`&mut [u8]`). When `alloc` is available, you can also pass
`RxStorage::Owned(Vec<u8>)`.
