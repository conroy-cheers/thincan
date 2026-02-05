//! `can-uds`: utilities for CAN identifier schemes commonly used with UDS (Unified Diagnostic
//! Services).
//!
//! This crate intentionally does **not** implement ISO-TP. It only defines helpers for encoding
//! and decoding CAN identifiers and building acceptance filters for those identifier schemes.
//!
//! # 29-bit "normal fixed" / physical addressing
//! A widely-used diagnostics scheme uses 29-bit identifiers of the form:
//! - `0x18DA_TA_SA` where `TA` is the **target address** and `SA` is the **source address**.
//!
//! The helpers in [`uds29`] are small and `no_std`-friendly.

#![no_std]

pub mod uds29 {
    use embedded_can::{ExtendedId, Id as CanId};
    use embedded_can_interface::{Id, IdMask, IdMaskFilter};

    /// Max 29-bit CAN identifier value.
    pub const EXT_ID_MAX: u32 = 0x1FFF_FFFF;

    /// 29-bit UDS addressing kind (normal fixed).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Uds29Kind {
        /// Physical addressing (PDU Format `0xDA`): `0x18DA_TA_SA`.
        Physical,
        /// Functional addressing (PDU Format `0xDB`): `0x18DB_TA_SA`.
        Functional,
    }

    /// Parsed 29-bit UDS addressing identifier (`0x18DA/0x18DB _TA_SA`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Uds29Id {
        pub kind: Uds29Kind,
        pub target: u8,
        pub source: u8,
    }

    /// UDS physical addressing base for 29-bit "normal fixed" addressing.
    pub const PHYS_BASE: u32 = 0x18DA_0000;
    /// UDS functional addressing base for 29-bit "normal fixed" addressing.
    pub const FUNC_BASE: u32 = 0x18DB_0000;

    /// Mask that matches the fixed "base" bits for the `0x18DA_TA_SA` / `0x18DB_TA_SA` patterns.
    pub const BASE_MASK: u32 = 0x1FFF_0000;
    /// Mask that matches a single `TA` but ignores `SA` (useful for "accept all senders to me").
    pub const TARGET_MASK: u32 = 0x1FFF_FF00;

    /// Encode a 29-bit UDS identifier as a raw `u32`.
    #[inline]
    pub const fn encode_id_raw(kind: Uds29Kind, target: u8, source: u8) -> u32 {
        let base = match kind {
            Uds29Kind::Physical => PHYS_BASE,
            Uds29Kind::Functional => FUNC_BASE,
        };
        base | ((target as u32) << 8) | (source as u32)
    }

    /// Decode a raw 29-bit CAN identifier of the form `0x18DA_TA_SA` / `0x18DB_TA_SA`.
    #[inline]
    pub const fn decode_id_raw(raw: u32) -> Option<Uds29Id> {
        if (raw & !EXT_ID_MAX) != 0 {
            return None;
        }
        let base = raw & BASE_MASK;
        let kind = if base == PHYS_BASE {
            Uds29Kind::Physical
        } else if base == FUNC_BASE {
            Uds29Kind::Functional
        } else {
            return None;
        };

        let target = ((raw >> 8) & 0xFF) as u8;
        let source = (raw & 0xFF) as u8;
        Some(Uds29Id {
            kind,
            target,
            source,
        })
    }

    /// Encode a 29-bit UDS identifier as an `embedded-can-interface` [`Id`].
    #[inline]
    pub fn encode_id(kind: Uds29Kind, target: u8, source: u8) -> Id {
        let raw = encode_id_raw(kind, target, source);
        Id::Extended(ExtendedId::new(raw).expect("UDS 29-bit ID must fit in 29 bits"))
    }

    /// Decode an `embedded-can` [`Id`] as a 29-bit UDS identifier.
    #[inline]
    pub fn decode_id(id: CanId) -> Option<Uds29Id> {
        match id {
            CanId::Extended(ext) => decode_id_raw(ext.as_raw()),
            CanId::Standard(_) => None,
        }
    }

    /// Encode a 29-bit physical-addressing CAN identifier as a raw `u32` (`0x18DA_TA_SA`).
    #[inline]
    pub const fn encode_phys_id_raw(target: u8, source: u8) -> u32 {
        encode_id_raw(Uds29Kind::Physical, target, source)
    }

    /// Encode a 29-bit functional-addressing CAN identifier as a raw `u32` (`0x18DB_TA_SA`).
    #[inline]
    pub const fn encode_func_id_raw(target: u8, source: u8) -> u32 {
        encode_id_raw(Uds29Kind::Functional, target, source)
    }

    /// Decode a raw 29-bit CAN identifier of the form `0x18DA_TA_SA`.
    #[inline]
    pub const fn decode_phys_id_raw(raw: u32) -> Option<(u8, u8)> {
        match decode_id_raw(raw) {
            Some(Uds29Id {
                kind: Uds29Kind::Physical,
                target,
                source,
            }) => Some((target, source)),
            _ => None,
        }
    }

    /// Decode a raw 29-bit CAN identifier of the form `0x18DB_TA_SA`.
    #[inline]
    pub const fn decode_func_id_raw(raw: u32) -> Option<(u8, u8)> {
        match decode_id_raw(raw) {
            Some(Uds29Id {
                kind: Uds29Kind::Functional,
                target,
                source,
            }) => Some((target, source)),
            _ => None,
        }
    }

    /// Encode a 29-bit physical-addressing CAN identifier as an `embedded-can-interface` [`Id`].
    #[inline]
    pub fn encode_phys_id(target: u8, source: u8) -> Id {
        encode_id(Uds29Kind::Physical, target, source)
    }

    /// Encode a 29-bit functional-addressing CAN identifier as an `embedded-can-interface` [`Id`].
    #[inline]
    pub fn encode_func_id(target: u8, source: u8) -> Id {
        encode_id(Uds29Kind::Functional, target, source)
    }

    /// Decode an `embedded-can` [`Id`] as a 29-bit UDS physical identifier.
    #[inline]
    pub fn decode_phys_id(id: CanId) -> Option<(u8, u8)> {
        decode_id(id).and_then(|v| match v.kind {
            Uds29Kind::Physical => Some((v.target, v.source)),
            Uds29Kind::Functional => None,
        })
    }

    /// Decode an `embedded-can` [`Id`] as a 29-bit UDS functional identifier.
    #[inline]
    pub fn decode_func_id(id: CanId) -> Option<(u8, u8)> {
        decode_id(id).and_then(|v| match v.kind {
            Uds29Kind::Functional => Some((v.target, v.source)),
            Uds29Kind::Physical => None,
        })
    }

    /// A small helper for returning 1 or 2 acceptance filters without allocation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AcceptanceFilters {
        filters: [IdMaskFilter; 2],
        len: usize,
    }

    impl AcceptanceFilters {
        #[inline]
        pub fn as_slice(&self) -> &[IdMaskFilter] {
            &self.filters[..self.len]
        }
    }

    fn filter_for_target_kind(kind: Uds29Kind, target: u8) -> IdMaskFilter {
        let base = match kind {
            Uds29Kind::Physical => PHYS_BASE,
            Uds29Kind::Functional => FUNC_BASE,
        };
        let raw = base | ((target as u32) << 8);
        IdMaskFilter {
            id: Id::Extended(ExtendedId::new(raw).expect("UDS base+target must fit in 29 bits")),
            mask: IdMask::Extended(TARGET_MASK),
        }
    }

    /// Build an acceptance filter that matches any physical-addressing ID with `target`.
    #[inline]
    pub fn filter_phys_for_target(target: u8) -> IdMaskFilter {
        filter_for_target_kind(Uds29Kind::Physical, target)
    }

    /// Build an acceptance filter that matches any functional-addressing ID with `target`.
    #[inline]
    pub fn filter_func_for_target(target: u8) -> IdMaskFilter {
        filter_for_target_kind(Uds29Kind::Functional, target)
    }

    /// Build acceptance filters for:
    /// - all physical frames addressed to `phys_target`, and
    /// - (optionally) all functional frames addressed to `func_target`.
    #[inline]
    pub fn filters_for_targets(phys_target: u8, func_target: Option<u8>) -> AcceptanceFilters {
        let phys = filter_phys_for_target(phys_target);
        if let Some(ft) = func_target {
            let func = filter_func_for_target(ft);
            AcceptanceFilters {
                filters: [phys, func],
                len: 2,
            }
        } else {
            AcceptanceFilters {
                filters: [phys, phys],
                len: 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::uds29;
    use embedded_can::{ExtendedId, Id as CanId};

    #[test]
    fn uds29_round_trip_raw() {
        let (ta, sa) = (0xF1u8, 0x33u8);
        let raw = uds29::encode_phys_id_raw(ta, sa);
        assert_eq!(raw, 0x18DA_F133);
        assert_eq!(uds29::decode_phys_id_raw(raw), Some((ta, sa)));
    }

    #[test]
    fn uds29_functional_round_trip_raw() {
        let (ta, sa) = (0x33u8, 0xF1u8);
        let raw = uds29::encode_func_id_raw(ta, sa);
        assert_eq!(raw, 0x18DB_33F1);
        assert_eq!(uds29::decode_func_id_raw(raw), Some((ta, sa)));
    }

    #[test]
    fn uds29_decode_rejects_non_extended_range() {
        let raw = 0xFFFF_FFFF;
        assert_eq!(uds29::decode_phys_id_raw(raw), None);
    }

    #[test]
    fn uds29_decode_rejects_wrong_base() {
        let raw = 0x18DB_0000 | (0x12u32 << 8) | 0x34;
        // This is functional, so physical decode must reject it.
        assert_eq!(uds29::decode_phys_id_raw(raw), None);
        assert_eq!(uds29::decode_func_id_raw(raw), Some((0x12, 0x34)));
    }

    #[test]
    fn uds29_decode_from_embedded_can_id() {
        let raw = uds29::encode_phys_id_raw(0xAA, 0x55);
        let id = CanId::Extended(ExtendedId::new(raw).unwrap());
        assert_eq!(uds29::decode_phys_id(id), Some((0xAA, 0x55)));
    }
}
