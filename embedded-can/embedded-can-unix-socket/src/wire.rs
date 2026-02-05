use crate::frame::UnixFrame;
use embedded_can::Id;
use embedded_can_interface::{Id as IfaceId, IdMask, IdMaskFilter};
use std::io::{self, Read, Write};

pub const MSG_SEND_FRAME: u8 = 1;
pub const MSG_FRAME: u8 = 2;
pub const MSG_ACK: u8 = 3;
pub const MSG_SET_FILTERS: u8 = 4;
pub const MSG_HELLO: u8 = 5;
pub const MSG_FILTERS_ACK: u8 = 6;

pub const ACK_OK: u8 = 0;
pub const ACK_NO_ACK: u8 = 1;
pub const ACK_BAD_FRAME: u8 = 2;
pub const ACK_SERVER_ERR: u8 = 3;

const FLAG_REMOTE: u8 = 1 << 0;
const FLAG_EXTENDED: u8 = 1 << 1;
const FLAG_EXPECT_ACK: u8 = 1 << 2;

pub const MAX_PAYLOAD_LEN: usize = 4096;
pub const MAX_DATA_LEN: usize = 64;
pub const SEND_FRAME_HDR_LEN: usize = 14;
pub const FRAME_HDR_LEN: usize = 6;
pub const ACK_LEN: usize = 9;

pub fn write_msg<W: Write>(writer: &mut W, msg_type: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload too large",
        ));
    }
    let mut header = [0u8; 5];
    header[0] = msg_type;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)
}

pub fn read_msg_into<R: Read>(reader: &mut R, payload: &mut Vec<u8>) -> io::Result<u8> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let msg_type = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload exceeds limit",
        ));
    }
    payload.clear();
    payload.resize(len, 0);
    reader.read_exact(payload)?;
    Ok(msg_type)
}

pub struct SendFrame {
    pub seq: u64,
    pub expect_ack: bool,
    pub frame: UnixFrame,
}

pub fn encode_send_frame_into(
    out: &mut [u8; SEND_FRAME_HDR_LEN + MAX_DATA_LEN],
    seq: u64,
    expect_ack: bool,
    frame: &UnixFrame,
) -> usize {
    out[0..8].copy_from_slice(&seq.to_le_bytes());
    out[8] = frame_flags(frame, expect_ack);
    out[9..13].copy_from_slice(&frame_id_raw(frame.id()).to_le_bytes());
    out[13] = frame.dlc_u8();
    let dlc = frame.dlc_u8() as usize;
    out[14..14 + dlc].copy_from_slice(&frame.data_bytes()[..dlc]);
    SEND_FRAME_HDR_LEN + dlc
}

pub fn decode_send_frame(payload: &[u8]) -> Result<SendFrame, &'static str> {
    if payload.len() < SEND_FRAME_HDR_LEN {
        return Err("invalid send frame payload length");
    }
    let seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let flags = payload[8];
    let id_raw = u32::from_le_bytes(payload[9..13].try_into().unwrap());
    let dlc = payload[13];
    let dlc_usize = dlc as usize;
    if dlc_usize > MAX_DATA_LEN {
        return Err("invalid dlc");
    }
    let expected = SEND_FRAME_HDR_LEN + dlc_usize;
    if payload.len() != expected {
        return Err("invalid send frame payload length");
    }
    let mut data = [0u8; MAX_DATA_LEN];
    data[..dlc_usize].copy_from_slice(&payload[14..14 + dlc_usize]);
    let frame = decode_frame_from_parts(flags, id_raw, dlc, data)?;
    Ok(SendFrame {
        seq,
        expect_ack: flags & FLAG_EXPECT_ACK != 0,
        frame,
    })
}

pub fn encode_frame_into(out: &mut [u8; FRAME_HDR_LEN + MAX_DATA_LEN], frame: &UnixFrame) -> usize {
    out[0] = frame_flags(frame, false);
    out[1..5].copy_from_slice(&frame_id_raw(frame.id()).to_le_bytes());
    out[5] = frame.dlc_u8();
    let dlc = frame.dlc_u8() as usize;
    out[6..6 + dlc].copy_from_slice(&frame.data_bytes()[..dlc]);
    FRAME_HDR_LEN + dlc
}

pub fn decode_frame(payload: &[u8]) -> Result<UnixFrame, &'static str> {
    if payload.len() < FRAME_HDR_LEN {
        return Err("invalid frame payload length");
    }
    let flags = payload[0];
    let id_raw = u32::from_le_bytes(payload[1..5].try_into().unwrap());
    let dlc = payload[5];
    let dlc_usize = dlc as usize;
    if dlc_usize > MAX_DATA_LEN {
        return Err("invalid dlc");
    }
    let expected = FRAME_HDR_LEN + dlc_usize;
    if payload.len() != expected {
        return Err("invalid frame payload length");
    }
    let mut data = [0u8; MAX_DATA_LEN];
    data[..dlc_usize].copy_from_slice(&payload[6..6 + dlc_usize]);
    decode_frame_from_parts(flags, id_raw, dlc, data)
}

pub fn encode_ack_into(out: &mut [u8; ACK_LEN], seq: u64, status: u8) {
    out[0..8].copy_from_slice(&seq.to_le_bytes());
    out[8] = status;
}

pub fn decode_ack(payload: &[u8]) -> Result<(u64, u8), &'static str> {
    if payload.len() != ACK_LEN {
        return Err("invalid ack payload length");
    }
    let seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let status = payload[8];
    Ok((seq, status))
}

pub fn encode_filters(seq: u64, filters: &[IdMaskFilter]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(10 + filters.len() * 9);
    let count = filters.len() as u16;
    payload.extend_from_slice(&seq.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    for filter in filters {
        let (flags, id_raw, mask_raw) = match filter.id {
            IfaceId::Standard(id) => match filter.mask {
                IdMask::Standard(mask) => (0u8, id.as_raw() as u32, mask as u32),
                IdMask::Extended(_) => (0u8, id.as_raw() as u32, 0),
            },
            IfaceId::Extended(id) => match filter.mask {
                IdMask::Extended(mask) => (FLAG_EXTENDED, id.as_raw(), mask),
                IdMask::Standard(_) => (FLAG_EXTENDED, id.as_raw(), 0),
            },
        };
        payload.push(flags);
        payload.extend_from_slice(&id_raw.to_le_bytes());
        payload.extend_from_slice(&mask_raw.to_le_bytes());
    }
    payload
}

pub fn decode_filters(payload: &[u8]) -> Result<(u64, Vec<IdMaskFilter>), &'static str> {
    if payload.len() < 10 {
        return Err("invalid filters payload length");
    }
    let seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let count = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
    let expected = 10 + count * 9;
    if payload.len() != expected {
        return Err("invalid filters payload length");
    }
    let mut filters = Vec::with_capacity(count);
    let mut offset = 10;
    for _ in 0..count {
        let flags = payload[offset];
        let id_raw = u32::from_le_bytes(payload[offset + 1..offset + 5].try_into().unwrap());
        let mask_raw = u32::from_le_bytes(payload[offset + 5..offset + 9].try_into().unwrap());
        offset += 9;
        if flags & FLAG_EXTENDED != 0 {
            let id = embedded_can::ExtendedId::new(id_raw).ok_or("invalid extended id")?;
            filters.push(IdMaskFilter {
                id: IfaceId::Extended(id),
                mask: IdMask::Extended(mask_raw),
            });
        } else {
            let id = embedded_can::StandardId::new(id_raw as u16).ok_or("invalid standard id")?;
            filters.push(IdMaskFilter {
                id: IfaceId::Standard(id),
                mask: IdMask::Standard(mask_raw as u16),
            });
        }
    }
    Ok((seq, filters))
}

fn frame_flags(frame: &UnixFrame, expect_ack: bool) -> u8 {
    let mut flags = 0u8;
    if frame.is_remote() {
        flags |= FLAG_REMOTE;
    }
    if matches!(frame.id(), Id::Extended(_)) {
        flags |= FLAG_EXTENDED;
    }
    if expect_ack {
        flags |= FLAG_EXPECT_ACK;
    }
    flags
}

fn frame_id_raw(id: Id) -> u32 {
    match id {
        Id::Standard(id) => id.as_raw() as u32,
        Id::Extended(id) => id.as_raw(),
    }
}

fn decode_frame_from_parts(
    flags: u8,
    id_raw: u32,
    dlc: u8,
    data: [u8; MAX_DATA_LEN],
) -> Result<UnixFrame, &'static str> {
    let remote = flags & FLAG_REMOTE != 0;
    let id = if flags & FLAG_EXTENDED != 0 {
        let id = embedded_can::ExtendedId::new(id_raw).ok_or("invalid extended id")?;
        Id::Extended(id)
    } else {
        let id = embedded_can::StandardId::new(id_raw as u16).ok_or("invalid standard id")?;
        Id::Standard(id)
    };
    UnixFrame::from_parts(id, dlc, data, remote).ok_or("invalid frame")
}
