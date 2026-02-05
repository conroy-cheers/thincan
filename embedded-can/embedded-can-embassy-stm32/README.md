# embedded-can-embassy-stm32

`embedded-can-interface` adapters for `embassy-stm32` CAN peripherals.

This crate provides small wrapper types that implement:

- `embedded_can_interface::AsyncTxFrameIo`
- `embedded_can_interface::AsyncRxFrameIo`

and (where supported by the peripheral):

- `embedded_can_interface::FilterConfig`

## Usage

Enable the `embassy-stm32` feature and depend on `embassy-stm32` in your application with the
appropriate chip features.

This crate does **not** select an STM32 chip for you; your application must do that via its own
`embassy-stm32` dependency.

### Optional features

- `bxcan`: enables `FilterConfig` for `EmbassyStm32Rx` using the BxCAN filter bank API, plus
  `wrap_split_bxcan`.
- `fdcan`: enables `EmbassyStm32FdProperties` with `FilterConfig` using the FDCAN message RAM
  filter tables, plus `wrap_split_fdcan`.
