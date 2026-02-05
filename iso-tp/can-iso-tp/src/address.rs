//! Helpers for ISO-TP addressing modes.

use embedded_can::ExtendedId;
use embedded_can_interface::Id;

use crate::config::IsoTpConfig;
use crate::errors::IsoTpError;

/// Address type used in 29-bit fixed/mixed addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAddressType {
    /// 1-to-1 communication (physical addressing).
    Physical,
    /// 1-to-n communication (functional addressing).
    Functional,
}

/// Transmit addressing parameters (CAN ID + optional first payload byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxAddress {
    /// CAN identifier used for transmission.
    pub id: Id,
    /// Optional ISO-TP addressing byte that is placed at the start of the CAN payload.
    ///
    /// When set, the Protocol Control Information (PCI) starts at byte offset 1.
    pub addr: Option<u8>,
}

/// Receive addressing parameters (CAN ID + optional expected first payload byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxAddress {
    /// CAN identifier expected when receiving.
    pub id: Id,
    /// Optional ISO-TP addressing byte that must match the first payload byte.
    ///
    /// When set, the Protocol Control Information (PCI) is expected at byte offset 1.
    pub addr: Option<u8>,
}

/// Combine a transmit and receive addressing scheme (asymmetric addressing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsymmetricAddress {
    /// Transmit addressing parameters.
    pub tx: TxAddress,
    /// Receive addressing parameters.
    pub rx: RxAddress,
}

/// Addressing parameters for a single ISO-TP node.
///
/// This is a convenient “fully expanded” form that can be converted into an [`IsoTpConfig`] via
/// `IsoTpConfig::from(IsoTpAddress)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoTpAddress {
    /// Transmit CAN identifier.
    pub tx_id: Id,
    /// Receive CAN identifier.
    pub rx_id: Id,
    /// Optional transmit addressing byte.
    pub tx_addr: Option<u8>,
    /// Optional receive addressing byte.
    pub rx_addr: Option<u8>,
}

fn ext_id(raw: u32) -> Result<Id, IsoTpError<()>> {
    ExtendedId::new(raw)
        .map(Id::Extended)
        .ok_or(IsoTpError::InvalidConfig)
}

fn fixed_base(target_type: TargetAddressType, physical_base: u32, functional_base: u32) -> u32 {
    match target_type {
        TargetAddressType::Physical => physical_base,
        TargetAddressType::Functional => functional_base,
    }
}

impl TxAddress {
    /// Normal addressing (no extra addressing byte).
    pub fn normal(id: Id) -> Self {
        Self { id, addr: None }
    }

    /// Extended addressing (adds a target address byte when sending).
    pub fn extended(id: Id, target_address: u8) -> Self {
        Self {
            id,
            addr: Some(target_address),
        }
    }

    /// Mixed 11-bit addressing (adds an address extension byte when sending).
    pub fn mixed_11(id: Id, address_extension: u8) -> Self {
        Self {
            id,
            addr: Some(address_extension),
        }
    }

    /// Normal fixed 29-bit addressing.
    ///
    /// This constructs a 29-bit CAN ID according to ISO-TP’s fixed addressing scheme.
    pub fn normal_fixed_29(
        source_address: u8,
        target_address: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        let base = fixed_base(target_type, 0x18DA0000, 0x18DB0000);
        let raw = base | ((target_address as u32) << 8) | source_address as u32;
        Ok(Self {
            id: ext_id(raw)?,
            addr: None,
        })
    }

    /// Mixed 29-bit addressing (fixed CAN ID + address extension byte).
    pub fn mixed_29(
        source_address: u8,
        target_address: u8,
        address_extension: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        let base = fixed_base(target_type, 0x18CE0000, 0x18CD0000);
        let raw = base | ((target_address as u32) << 8) | source_address as u32;
        Ok(Self {
            id: ext_id(raw)?,
            addr: Some(address_extension),
        })
    }
}

impl RxAddress {
    /// Normal addressing (no extra addressing byte).
    pub fn normal(id: Id) -> Self {
        Self { id, addr: None }
    }

    /// Extended addressing (expects a source address byte when receiving).
    pub fn extended(id: Id, source_address: u8) -> Self {
        Self {
            id,
            addr: Some(source_address),
        }
    }

    /// Mixed 11-bit addressing (expects an address extension byte when receiving).
    pub fn mixed_11(id: Id, address_extension: u8) -> Self {
        Self {
            id,
            addr: Some(address_extension),
        }
    }

    /// Normal fixed 29-bit addressing.
    ///
    /// This constructs a 29-bit CAN ID according to ISO-TP’s fixed addressing scheme.
    pub fn normal_fixed_29(
        source_address: u8,
        target_address: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        let base = fixed_base(target_type, 0x18DA0000, 0x18DB0000);
        let raw = base | ((source_address as u32) << 8) | target_address as u32;
        Ok(Self {
            id: ext_id(raw)?,
            addr: None,
        })
    }

    /// Mixed 29-bit addressing (fixed CAN ID + address extension byte).
    pub fn mixed_29(
        source_address: u8,
        target_address: u8,
        address_extension: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        let base = fixed_base(target_type, 0x18CE0000, 0x18CD0000);
        let raw = base | ((source_address as u32) << 8) | target_address as u32;
        Ok(Self {
            id: ext_id(raw)?,
            addr: Some(address_extension),
        })
    }
}

impl AsymmetricAddress {
    /// Construct a new asymmetric address pair.
    pub fn new(tx: TxAddress, rx: RxAddress) -> Self {
        Self { tx, rx }
    }
}

impl From<AsymmetricAddress> for IsoTpAddress {
    fn from(value: AsymmetricAddress) -> Self {
        Self {
            tx_id: value.tx.id,
            rx_id: value.rx.id,
            tx_addr: value.tx.addr,
            rx_addr: value.rx.addr,
        }
    }
}

impl IsoTpAddress {
    /// Normal addressing with explicit Tx/Rx IDs.
    pub fn normal(tx_id: Id, rx_id: Id) -> Self {
        Self {
            tx_id,
            rx_id,
            tx_addr: None,
            rx_addr: None,
        }
    }

    /// Extended addressing with explicit Tx/Rx IDs and explicit source/target address bytes.
    pub fn extended(tx_id: Id, rx_id: Id, source_address: u8, target_address: u8) -> Self {
        Self {
            tx_id,
            rx_id,
            tx_addr: Some(target_address),
            rx_addr: Some(source_address),
        }
    }

    /// Mixed 11-bit addressing with explicit Tx/Rx IDs and a shared address extension byte.
    pub fn mixed_11(tx_id: Id, rx_id: Id, address_extension: u8) -> Self {
        Self {
            tx_id,
            rx_id,
            tx_addr: Some(address_extension),
            rx_addr: Some(address_extension),
        }
    }

    /// Normal fixed 29-bit addressing.
    pub fn normal_fixed_29(
        source_address: u8,
        target_address: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        Ok(Self::from(AsymmetricAddress::new(
            TxAddress::normal_fixed_29(source_address, target_address, target_type)?,
            RxAddress::normal_fixed_29(source_address, target_address, target_type)?,
        )))
    }

    /// Mixed 29-bit addressing.
    pub fn mixed_29(
        source_address: u8,
        target_address: u8,
        address_extension: u8,
        target_type: TargetAddressType,
    ) -> Result<Self, IsoTpError<()>> {
        Ok(Self::from(AsymmetricAddress::new(
            TxAddress::mixed_29(
                source_address,
                target_address,
                address_extension,
                target_type,
            )?,
            RxAddress::mixed_29(
                source_address,
                target_address,
                address_extension,
                target_type,
            )?,
        )))
    }
}

impl From<IsoTpAddress> for IsoTpConfig {
    fn from(value: IsoTpAddress) -> Self {
        Self {
            tx_id: value.tx_id,
            rx_id: value.rx_id,
            tx_addr: value.tx_addr,
            rx_addr: value.rx_addr,
            ..IsoTpConfig::default()
        }
    }
}
