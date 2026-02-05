# can-uds

Small, `no_std`-friendly helpers for CAN identifier schemes commonly used with UDS (Unified
Diagnostic Services).

This crate intentionally does **not** implement ISO-TP. It only defines how to *encode/decode CAN
identifiers* and build acceptance filters for those schemes.

## 29-bit "normal fixed" / physical addressing (0x18DA_TA_SA)

A widely-used scheme for ISO-TP-over-CAN diagnostics uses 29-bit identifiers of the form:

- `0x18DA_TA_SA`
  - `TA` = target address (8-bit)
  - `SA` = source address (8-bit)

This crate provides:

- `uds29::encode_phys_id_raw(target, source) -> u32`
- `uds29::decode_phys_id_raw(raw) -> Option<(target, source)>`
- `uds29::encode_phys_id(target, source) -> embedded_can_interface::Id`
- `uds29::decode_phys_id(id) -> Option<(target, source)>`
- `uds29::filter_phys_for_target(target) -> embedded_can_interface::IdMaskFilter` (matches any source)

## Functional addressing (0x18DB_TA_SA)

The same scheme is also commonly used for functional (broadcast/group) requests:

- `0x18DB_TA_SA`
  - `TA` = functional target address (often a group / broadcast address)
  - `SA` = source address

Helpers:

- `uds29::encode_func_id_raw(target, source) -> u32`
- `uds29::decode_func_id_raw(raw) -> Option<(target, source)>`
- `uds29::filter_func_for_target(target) -> embedded_can_interface::IdMaskFilter`
