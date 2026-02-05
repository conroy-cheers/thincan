//! Transmit-side progress tracking for ISO-TP.

use core::time::Duration;

/// Progress indicator for non-blocking APIs.
///
/// Both the blocking and async APIs in this crate are built on top of polling-style primitives.
/// `Progress` describes the state after a single poll step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Transfer is ongoing.
    InFlight,
    /// Waiting for a flow control frame.
    WaitingForFlowControl,
    /// Transfer finished.
    Completed,
    /// Backend would block; caller should retry later.
    WouldBlock,
}

/// Bookkeeping for an in-flight segmented transfer.
///
/// This struct is public primarily for debugging and integration; most consumers interact through
/// [`crate::IsoTpNode`] / [`crate::IsoTpAsyncNode`] instead.
pub struct TxSession {
    /// Expected full payload length.
    pub payload_len: usize,
    /// Current offset into payload.
    pub offset: usize,
    /// Next sequence number nibble.
    pub next_sn: u8,
    /// Block size negotiated.
    pub block_size: u8,
    /// Frames remaining before next FC.
    pub block_remaining: u8,
    /// Separation time between CFs.
    pub st_min: Duration,
    /// Count of Wait responses seen.
    pub wait_count: u8,
}

impl TxSession {
    /// Build a new session with provided limits.
    pub fn new(payload_len: usize, block_size: u8, st_min: Duration) -> Self {
        let remaining = block_size;
        Self {
            payload_len,
            offset: 0,
            next_sn: 1,
            block_size,
            block_remaining: remaining,
            st_min,
            wait_count: 0,
        }
    }
}

/// Transmit state machine wrapper.
///
/// This is the internal state carried between [`crate::IsoTpNode::poll_send`] calls.
pub enum TxState<CInstant> {
    /// No active transfer.
    Idle,
    /// Sent First Frame; waiting for FC until deadline.
    WaitingForFc {
        session: TxSession,
        deadline: CInstant,
    },
    /// Sending consecutive frames; may be pacing by STmin.
    Sending {
        session: TxSession,
        st_min_deadline: Option<CInstant>,
    },
}
