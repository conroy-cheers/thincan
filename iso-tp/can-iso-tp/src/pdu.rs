//! Encode and decode ISO-TP protocol control information.

use core::time::Duration;
use embedded_can::Frame;
use embedded_can_interface::Id;

use crate::errors::IsoTpError;

/// Flow control status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowStatus {
    /// Clear to send more consecutive frames.
    ClearToSend,
    /// Wait before resuming.
    Wait,
    /// Abort due to overflow.
    Overflow,
}

/// Parsed ISO-TP Protocol Data Unit (PDU).
///
/// This enum is used by the encoder/decoder and by the send/receive state machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pdu<'a> {
    /// Single Frame payload.
    SingleFrame { len: u8, data: &'a [u8] },
    /// First Frame with total length and first bytes.
    FirstFrame { len: u16, data: &'a [u8] },
    /// Consecutive Frame carrying sequence and bytes.
    ConsecutiveFrame { sn: u8, data: &'a [u8] },
    /// Flow Control feedback.
    FlowControl {
        /// Flow status from receiver to sender.
        status: FlowStatus,
        /// Block size requested by receiver (0 = unlimited).
        block_size: u8,
        /// STmin (encoded byte form, not a `Duration`).
        st_min: u8,
    },
}

fn to_can_id(id: Id) -> embedded_can::Id {
    match id {
        Id::Standard(std_id) => embedded_can::Id::Standard(std_id),
        Id::Extended(ext_id) => embedded_can::Id::Extended(ext_id),
    }
}

fn pci_offset_from_prefix(prefix: Option<u8>) -> usize {
    usize::from(prefix.is_some())
}

/// Build a CAN frame representing the given PDU.
///
/// This uses “normal addressing” (no addressing prefix byte). For extended/mixed addressing, use
/// [`encode_with_prefix`].
pub fn encode<F: Frame>(id: Id, pdu: &Pdu<'_>, padding: Option<u8>) -> Result<F, IsoTpError<()>> {
    encode_with_prefix(id, pdu, padding, None)
}

/// Build a CAN frame representing the given PDU with an optional addressing prefix byte.
///
/// - `prefix = None` means the PCI starts at byte 0.
/// - `prefix = Some(x)` means byte 0 is the addressing byte and the PCI starts at byte 1.
pub fn encode_with_prefix<F: Frame>(
    id: Id,
    pdu: &Pdu<'_>,
    padding: Option<u8>,
    prefix: Option<u8>,
) -> Result<F, IsoTpError<()>> {
    encode_with_prefix_sized(id, pdu, padding, prefix, 8)
}

/// Like [`encode_with_prefix`], but allows a larger CAN payload length (e.g. CAN FD).
///
/// `frame_len` is the desired CAN payload size. Most classic CAN drivers only support 8.
pub fn encode_with_prefix_sized<F: Frame>(
    id: Id,
    pdu: &Pdu<'_>,
    padding: Option<u8>,
    prefix: Option<u8>,
    frame_len: usize,
) -> Result<F, IsoTpError<()>> {
    if !(8..=64).contains(&frame_len) {
        return Err(IsoTpError::InvalidFrame);
    }
    let mut buf = [0u8; 64];
    let offset = pci_offset_from_prefix(prefix);
    if offset > 0 {
        buf[0] = prefix.unwrap();
    }

    let used = match pdu {
        Pdu::SingleFrame { len, data } => {
            let payload_len = *len as usize;
            if payload_len > data.len() {
                return Err(IsoTpError::InvalidFrame);
            }

            let classic_max = 7usize.saturating_sub(offset);
            if payload_len <= classic_max {
                buf[offset] = *len & 0x0F;
                let used = offset + 1 + payload_len;
                buf[offset + 1..used].copy_from_slice(&data[..payload_len]);
                used
            } else {
                // CAN FD extended Single Frame length encoding.
                // SF_DL nibble = 0, next byte is 8-bit length.
                if frame_len <= 8 || payload_len > u8::MAX as usize {
                    return Err(IsoTpError::InvalidFrame);
                }
                let needed = offset + 2 + payload_len;
                if needed > frame_len {
                    return Err(IsoTpError::InvalidFrame);
                }
                buf[offset] = 0x00;
                buf[offset + 1] = payload_len as u8;
                buf[offset + 2..needed].copy_from_slice(&data[..payload_len]);
                needed
            }
        }
        Pdu::FirstFrame { len, data } => {
            let max_sf = if frame_len > 8 {
                frame_len.saturating_sub(2 + offset)
            } else {
                7usize.saturating_sub(offset)
            };
            if (*len as usize) <= max_sf || *len > 4095 || data.is_empty() {
                return Err(IsoTpError::InvalidFrame);
            }
            buf[offset] = 0x10 | ((*len >> 8) as u8 & 0x0F);
            buf[offset + 1] = (*len & 0xFF) as u8;
            let data_len = data.len().min(frame_len.saturating_sub(2 + offset));
            buf[offset + 2..offset + 2 + data_len].copy_from_slice(&data[..data_len]);
            offset + 2 + data_len
        }
        Pdu::ConsecutiveFrame { sn, data } => {
            if data.len() > frame_len.saturating_sub(1 + offset) {
                return Err(IsoTpError::InvalidFrame);
            }
            buf[offset] = 0x20 | (*sn & 0x0F);
            let used = offset + 1 + data.len();
            buf[offset + 1..used].copy_from_slice(data);
            used
        }
        Pdu::FlowControl {
            status,
            block_size,
            st_min,
        } => {
            let status_nibble = match status {
                FlowStatus::ClearToSend => 0x0,
                FlowStatus::Wait => 0x1,
                FlowStatus::Overflow => 0x2,
            };
            if offset + 3 > frame_len {
                return Err(IsoTpError::InvalidFrame);
            }
            buf[offset] = 0x30 | status_nibble;
            buf[offset + 1] = *block_size;
            buf[offset + 2] = *st_min;
            offset + 3
        }
    };

    let used = if let Some(pad) = padding {
        for b in buf[used..frame_len].iter_mut() {
            *b = pad;
        }
        frame_len
    } else {
        used
    };

    Frame::new(to_can_id(id), &buf[..used]).ok_or(IsoTpError::InvalidFrame)
}

/// Decode raw CAN data into a PDU view.
///
/// This interprets the PCI at byte offset 0. For extended/mixed addressing, use
/// [`decode_with_offset`] with `pci_offset = 1`.
pub fn decode<'a>(data: &'a [u8]) -> Result<Pdu<'a>, IsoTpError<()>> {
    decode_with_offset(data, 0)
}

/// Decode raw CAN data into a PDU view, interpreting the PCI at the provided byte offset.
///
/// `pci_offset` is expected to be 0 (normal addressing) or 1 (extended/mixed addressing). Other
/// offsets are rejected.
pub fn decode_with_offset<'a>(
    data: &'a [u8],
    pci_offset: usize,
) -> Result<Pdu<'a>, IsoTpError<()>> {
    if pci_offset > 1 || data.len() <= pci_offset {
        return Err(IsoTpError::InvalidFrame);
    }

    let pci = data[pci_offset] >> 4;
    match pci {
        0x0 => {
            let len = data[pci_offset] & 0x0F;
            if len == 0 && data.len() > 8 {
                // CAN FD extended Single Frame length encoding.
                if data.len() < pci_offset + 2 {
                    return Err(IsoTpError::InvalidFrame);
                }
                let payload_len = data[pci_offset + 1] as usize;
                let payload_start = pci_offset + 2;
                if data.len() < payload_start + payload_len {
                    return Err(IsoTpError::InvalidFrame);
                }
                Ok(Pdu::SingleFrame {
                    len: payload_len as u8,
                    data: &data[payload_start..payload_start + payload_len],
                })
            } else {
                if len as usize > 7 {
                    return Err(IsoTpError::InvalidFrame);
                }
                let payload_len = len as usize;
                let payload_start = pci_offset + 1;
                if data.len() < payload_start + payload_len {
                    return Err(IsoTpError::InvalidFrame);
                }
                Ok(Pdu::SingleFrame {
                    len,
                    data: &data[payload_start..payload_start + payload_len],
                })
            }
        }
        0x1 => {
            if data.len() < pci_offset + 2 {
                return Err(IsoTpError::InvalidFrame);
            }
            let len = (((data[pci_offset] & 0x0F) as u16) << 8) | data[pci_offset + 1] as u16;
            let max_sf = if data.len() > 8 {
                data.len().saturating_sub(2 + pci_offset)
            } else {
                7usize.saturating_sub(pci_offset)
            };
            if (len as usize) <= max_sf || len > 4095 {
                return Err(IsoTpError::InvalidFrame);
            }
            Ok(Pdu::FirstFrame {
                len,
                data: &data[pci_offset + 2..],
            })
        }
        0x2 => {
            let sn = data[pci_offset] & 0x0F;
            Ok(Pdu::ConsecutiveFrame {
                sn,
                data: &data[pci_offset + 1..],
            })
        }
        0x3 => {
            if data.len() < pci_offset + 3 {
                return Err(IsoTpError::InvalidFrame);
            }
            let status = match data[pci_offset] & 0x0F {
                0x0 => FlowStatus::ClearToSend,
                0x1 => FlowStatus::Wait,
                0x2 => FlowStatus::Overflow,
                _ => return Err(IsoTpError::InvalidFrame),
            };
            Ok(Pdu::FlowControl {
                status,
                block_size: data[pci_offset + 1],
                st_min: data[pci_offset + 2],
            })
        }
        _ => Err(IsoTpError::InvalidFrame),
    }
}

/// Convert STmin byte to a Duration, returning None for reserved values.
pub fn st_min_to_duration(raw: u8) -> Option<Duration> {
    match raw {
        0x00..=0x7F => Some(Duration::from_millis(raw as u64)),
        0xF1..=0xF9 => Some(Duration::from_micros((raw as u64 - 0xF0) * 100)),
        _ => None,
    }
}

/// Encode a Duration into an STmin byte, clamping to the supported range.
pub fn duration_to_st_min(duration: Duration) -> u8 {
    let micros = duration.as_micros();
    if micros == 0 {
        return 0;
    }
    if (100..=900).contains(&micros) && micros.is_multiple_of(100) {
        return 0xF0 + (micros / 100) as u8;
    }
    let millis = duration.as_millis();
    if millis <= 0x7F { millis as u8 } else { 0x7F }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_can::Id as CanId;
    use embedded_can::StandardId;
    use embedded_can_mock::MockFrame;

    fn sid(id: u16) -> Id {
        Id::Standard(StandardId::new(id).unwrap())
    }

    #[test]
    fn encode_and_decode_single_frame() {
        let pdu = Pdu::SingleFrame {
            len: 3,
            data: &[0xAA, 0xBB, 0xCC],
        };
        let frame: MockFrame = encode(sid(0x123), &pdu, None).unwrap();
        assert_eq!(frame.id(), CanId::Standard(StandardId::new(0x123).unwrap()));
        assert_eq!(frame.data()[0], 0x03);
        let parsed = decode(frame.data()).unwrap();
        match parsed {
            Pdu::SingleFrame { len, data } => {
                assert_eq!(len, 3);
                assert_eq!(data, &[0xAA, 0xBB, 0xCC]);
            }
            _ => panic!("wrong PDU decoded"),
        }
    }

    #[test]
    fn encode_and_decode_first_and_consecutive() {
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let ff: MockFrame = encode(
            sid(0x201),
            &Pdu::FirstFrame {
                len: payload.len() as u16,
                data: &payload,
            },
            Some(0xAA),
        )
        .unwrap();
        let parsed = decode(ff.data()).unwrap();
        match parsed {
            Pdu::FirstFrame { len, data } => {
                assert_eq!(len, payload.len() as u16);
                assert_eq!(data, &payload[..6]);
            }
            _ => panic!("unexpected PDU"),
        }

        let cf: MockFrame = encode(
            sid(0x201),
            &Pdu::ConsecutiveFrame {
                sn: 1,
                data: &payload[6..],
            },
            None,
        )
        .unwrap();
        let parsed_cf = decode(cf.data()).unwrap();
        match parsed_cf {
            Pdu::ConsecutiveFrame { sn, data } => {
                assert_eq!(sn, 1);
                assert_eq!(data, &payload[6..]);
            }
            _ => panic!("unexpected PDU"),
        }
    }

    #[test]
    fn flow_control_roundtrip() {
        let frame: MockFrame = encode(
            sid(0x333),
            &Pdu::FlowControl {
                status: FlowStatus::ClearToSend,
                block_size: 4,
                st_min: 10,
            },
            None,
        )
        .unwrap();
        let parsed = decode(frame.data()).unwrap();
        match parsed {
            Pdu::FlowControl {
                status,
                block_size,
                st_min,
            } => {
                assert_eq!(status, FlowStatus::ClearToSend);
                assert_eq!(block_size, 4);
                assert_eq!(st_min, 10);
            }
            _ => panic!("unexpected PDU"),
        }
    }

    #[test]
    fn decode_rejects_short_first_frame() {
        let data = [0x10, 0x00];
        assert!(decode(&data).is_err());
    }

    #[test]
    fn st_min_reserved_values_return_none() {
        assert!(st_min_to_duration(0x80).is_none());
        assert!(st_min_to_duration(0xF0).is_none());
    }
}
