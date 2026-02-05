//! ISO-TP configuration container.

use core::time::Duration;
use embedded_can_interface::Id;

/// Configuration for an ISO-TP node.
#[derive(Debug, Clone)]
pub struct IsoTpConfig {
    /// CAN identifier used when transmitting ISO-TP PDUs.
    pub tx_id: Id,
    /// CAN identifier expected when receiving ISO-TP PDUs.
    pub rx_id: Id,
    /// Optional first payload byte to prepend to transmitted frames (extended/mixed addressing).
    pub tx_addr: Option<u8>,
    /// Optional first payload byte expected on received frames (extended/mixed addressing).
    pub rx_addr: Option<u8>,
    /// Number of consecutive frames allowed before requesting a new flow control (0 = unlimited).
    pub block_size: u8,
    /// Minimum separation time enforced between consecutive frames.
    pub st_min: Duration,
    /// Maximum number of FlowControl::Wait responses accepted before failing.
    pub wft_max: u8,
    /// Optional padding byte for transmitted frames (None = no padding).
    pub padding: Option<u8>,
    /// Maximum application payload length accepted.
    pub max_payload_len: usize,
    /// Timeout waiting for transmit availability.
    pub n_as: Duration,
    /// Timeout waiting for receive availability.
    pub n_ar: Duration,
    /// Timeout waiting for flow control after First Frame.
    pub n_bs: Duration,
    /// Timeout waiting for Consecutive Frame while receiving.
    pub n_br: Duration,
    /// Timeout between consecutive frame transmissions.
    pub n_cs: Duration,
    /// Maximum CAN payload size (DLC) supported by the backend.
    ///
    /// - Classic CAN uses 8 bytes.
    /// - CAN FD can carry up to 64 bytes (if supported by the backend frame type).
    pub frame_len: usize,
}

impl Default for IsoTpConfig {
    /// Baseline config with zeroed IDs and 4 KB payload limits.
    fn default() -> Self {
        Self {
            tx_id: Id::Standard(embedded_can::StandardId::new(0).unwrap()),
            rx_id: Id::Standard(embedded_can::StandardId::new(0).unwrap()),
            tx_addr: None,
            rx_addr: None,
            block_size: 0,
            st_min: Duration::from_millis(0),
            wft_max: 0,
            padding: None,
            max_payload_len: 4095,
            n_as: Duration::from_millis(1000),
            n_ar: Duration::from_millis(1000),
            n_bs: Duration::from_millis(1000),
            n_br: Duration::from_millis(1000),
            n_cs: Duration::from_millis(1000),
            frame_len: 8,
        }
    }
}

impl IsoTpConfig {
    /// Reject invalid limits or mirrored IDs.
    #[allow(clippy::result_unit_err)]
    pub fn validate(&self) -> Result<(), ()> {
        if self.max_payload_len == 0 || self.max_payload_len > 4095 {
            return Err(());
        }
        if !(8..=64).contains(&self.frame_len) {
            return Err(());
        }
        if self.tx_id == self.rx_id
            && (self.tx_addr.is_none() || self.rx_addr.is_none() || self.tx_addr == self.rx_addr)
        {
            return Err(());
        }
        Ok(())
    }

    /// Index within an outgoing CAN payload where the PCI starts.
    pub fn tx_pci_offset(&self) -> usize {
        usize::from(self.tx_addr.is_some())
    }

    /// Index within an incoming CAN payload where the PCI starts.
    pub fn rx_pci_offset(&self) -> usize {
        usize::from(self.rx_addr.is_some())
    }

    /// Max application bytes in a Single Frame under the configured transmit addressing.
    pub fn max_single_frame_payload(&self) -> usize {
        let offset = self.tx_pci_offset();
        if self.frame_len > 8 {
            self.frame_len.saturating_sub(2 + offset)
        } else {
            7usize.saturating_sub(offset)
        }
    }

    /// Max application bytes carried in a First Frame under the configured transmit addressing.
    pub fn max_first_frame_payload(&self) -> usize {
        self.frame_len.saturating_sub(2 + self.tx_pci_offset())
    }

    /// Max application bytes carried in a Consecutive Frame under the configured transmit addressing.
    pub fn max_consecutive_frame_payload(&self) -> usize {
        self.frame_len.saturating_sub(1 + self.tx_pci_offset())
    }
}
