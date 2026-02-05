use embedded_can::{Frame as EmbeddedFrame, Id};

const MAX_DLC: usize = 64;

/// A CAN 2.0 frame transported over the Unix-domain bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnixFrame {
    id: Id,
    data: [u8; MAX_DLC],
    dlc: u8,
    remote: bool,
}

impl UnixFrame {
    /// Returns the CAN identifier for this frame.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Returns true if this is a remote frame.
    pub fn is_remote(&self) -> bool {
        self.remote
    }
}

impl EmbeddedFrame for UnixFrame {
    fn new(id: impl Into<Id>, data: &[u8]) -> Option<Self> {
        if data.len() > MAX_DLC {
            return None;
        }
        let mut buf = [0u8; MAX_DLC];
        buf[..data.len()].copy_from_slice(data);
        Some(Self {
            id: id.into(),
            data: buf,
            dlc: data.len() as u8,
            remote: false,
        })
    }

    fn new_remote(id: impl Into<Id>, dlc: usize) -> Option<Self> {
        if dlc > MAX_DLC {
            return None;
        }
        Some(Self {
            id: id.into(),
            data: [0u8; MAX_DLC],
            dlc: dlc as u8,
            remote: true,
        })
    }

    fn is_extended(&self) -> bool {
        matches!(self.id, Id::Extended(_))
    }

    fn is_remote_frame(&self) -> bool {
        self.remote
    }

    fn id(&self) -> Id {
        self.id
    }

    fn dlc(&self) -> usize {
        self.dlc as usize
    }

    fn data(&self) -> &[u8] {
        if self.remote {
            &[]
        } else {
            &self.data[..self.dlc as usize]
        }
    }
}

impl UnixFrame {
    pub(crate) fn dlc_u8(&self) -> u8 {
        self.dlc
    }

    pub(crate) fn data_bytes(&self) -> [u8; MAX_DLC] {
        self.data
    }

    pub(crate) fn from_parts(id: Id, dlc: u8, data: [u8; MAX_DLC], remote: bool) -> Option<Self> {
        if dlc as usize > MAX_DLC {
            return None;
        }
        Some(Self {
            id,
            data,
            dlc,
            remote,
        })
    }
}
