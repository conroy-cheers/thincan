use core::time::Duration;

use can_isotp_interface::SendError;

#[derive(Debug)]
pub enum QueueIsoTpError<E> {
    PayloadTooLarge { needed: usize, capacity: usize },
    Transport(E),
    ChannelClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    TxReq,
    TxResp,
    RxEvt,
}

#[derive(Clone, Copy, Debug)]
pub struct ActorConfig {
    pub recv_poll_timeout: Duration,
    pub tx_burst_limit: usize,
    pub fixed_reply_to: Option<u8>,
    pub allowed_reply_from: Option<u8>,
}

impl ActorConfig {
    pub const fn new(
        recv_poll_timeout: Duration,
        tx_burst_limit: usize,
        fixed_reply_to: Option<u8>,
    ) -> Self {
        Self {
            recv_poll_timeout,
            tx_burst_limit,
            fixed_reply_to,
            allowed_reply_from: None,
        }
    }

    pub const fn with_fixed_reply_to(mut self, fixed_reply_to: Option<u8>) -> Self {
        self.fixed_reply_to = fixed_reply_to;
        self
    }

    pub const fn with_allowed_reply_from(mut self, allowed_reply_from: Option<u8>) -> Self {
        self.allowed_reply_from = allowed_reply_from;
        self
    }

    pub const fn normalized_tx_burst_limit(&self) -> usize {
        if self.tx_burst_limit == 0 {
            1
        } else {
            self.tx_burst_limit
        }
    }
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            recv_poll_timeout: Duration::from_millis(1),
            tx_burst_limit: 8,
            fixed_reply_to: None,
            allowed_reply_from: None,
        }
    }
}

pub trait ActorHooks<E> {
    fn on_actor_started(&self) {}

    fn on_tx_request(&self, _to: u8, _is_functional: bool, _len: usize, _timeout: Duration) {}

    fn on_tx_result(&self, _result: &Result<(), SendError<QueueIsoTpError<E>>>) {}

    fn on_rx_delivered(&self, _from: u8, _len: usize) {}

    fn on_rx_filtered_source(&self, _expected: u8, _got: u8, _len: usize) {}

    fn on_rx_buffer_too_small(&self, _needed: usize, _got: usize) {}

    fn on_rx_backend_error(&self, _err: &E) {}

    fn on_queue_backpressure(&self, _queue: QueueKind, _blocked_for: Duration) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopHooks;

impl<E> ActorHooks<E> for NoopHooks {}
