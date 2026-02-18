//! Simple log bundle for `thincan` (unidirectional text messages).
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(async_fn_in_trait)]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "embassy")]
use core::marker::PhantomData;
#[cfg(feature = "embassy")]
use core::time::Duration;

/// Re-exported for use by macros without requiring downstream crates to depend on `capnp`.
#[doc(hidden)]
pub use capnp;

#[cfg(feature = "embassy")]
#[doc(hidden)]
pub use embassy_sync;

/// Maximum log payload size (bytes).
pub const MAX_LOG_BYTES: usize = 1024;

/// Cap'n Proto "word padding" (Cap'n Proto Data blobs are padded to a word boundary).
pub const fn capnp_padded_len(len: usize) -> usize {
    (len + 7) & !7
}

/// Upper bound for an encoded `LogMsg` body (bytes).
pub const fn log_msg_max_encoded_len(text_len: usize) -> usize {
    32 + capnp_padded_len(text_len)
}

/// Convert a byte count to a conservative Cap'n Proto scratch requirement (words).
pub const fn capnp_scratch_words_for_bytes(bytes: usize) -> usize {
    (bytes + 7) / 8
}

capnp::generated_code!(pub mod log_capnp);
pub use log_capnp as schema;

/// Atlas contract for log message types.
pub trait Atlas {
    type LogMsg: thincan::Message;
}

/// Async enqueue helper used by bundle handlers.
pub trait AsyncEnqueue<T> {
    async fn send(&self, msg: T);
    fn try_send(&self, msg: T) -> Result<(), ()>;
}

/// Async dequeue helper used by outbound bus participants.
pub trait AsyncDequeue<T> {
    async fn recv(&mut self) -> T;
    fn try_recv(&mut self) -> Result<T, ()>;
}

#[cfg(feature = "embassy")]
impl<'a, M, T, const DEPTH: usize> AsyncEnqueue<T>
    for &'a embassy_sync::channel::Channel<M, T, DEPTH>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
{
    async fn send(&self, msg: T) {
        embassy_sync::channel::Channel::send(*self, msg).await;
    }

    fn try_send(&self, msg: T) -> Result<(), ()> {
        embassy_sync::channel::Channel::try_send(*self, msg).map_err(|_| ())
    }
}

#[cfg(feature = "embassy")]
impl<'a, M, T, const DEPTH: usize> AsyncDequeue<T>
    for &'a embassy_sync::channel::Channel<M, T, DEPTH>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
{
    async fn recv(&mut self) -> T {
        embassy_sync::channel::Channel::receive(*self).await
    }

    fn try_recv(&mut self) -> Result<T, ()> {
        embassy_sync::channel::Channel::try_receive(*self).map_err(|_| ())
    }
}

/// Inbound log event produced by bundle handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEvent<ReplyTo, const MAX: usize> {
    pub reply_to: ReplyTo,
    pub len: u16,
    pub data: [u8; MAX],
    pub overflow: bool,
}

/// Outbound log payload to be transmitted by a bus participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundLog<ReplyTo, const MAX: usize> {
    pub reply_to: ReplyTo,
    pub len: u16,
    pub data: [u8; MAX],
    pub overflow: bool,
}

impl<ReplyTo: Copy, const MAX: usize> OutboundLog<ReplyTo, MAX> {
    pub fn new(reply_to: ReplyTo, text: &str) -> Self {
        use core::cmp::min;
        let bytes = text.as_bytes();
        let mut data = [0u8; MAX];
        let copy_len = min(bytes.len(), MAX);
        data[..copy_len].copy_from_slice(&bytes[..copy_len]);
        Self {
            reply_to,
            len: min(bytes.len(), u16::MAX as usize) as u16,
            data,
            overflow: bytes.len() > MAX,
        }
    }

    pub fn text_bytes(&self) -> &[u8] {
        let len = (self.len as usize).min(MAX);
        &self.data[..len]
    }
}

/// Receiver-side state access for log messages.
pub trait LogReceiverState<ReplyTo, const MAX: usize> {
    type InboxQueue: AsyncEnqueue<LogEvent<ReplyTo, MAX>> + Clone;

    fn log_inbox_queue(&self) -> Self::InboxQueue;
}

/// Sender-side state access for outbound log messages.
pub trait LogSenderState<ReplyTo, const MAX: usize> {
    type OutboxQueue: AsyncDequeue<OutboundLog<ReplyTo, MAX>> + Clone;

    fn log_outbox_queue(&self) -> Self::OutboxQueue;
}

/// Convenience receiver-side state wrapper.
#[derive(Debug, Clone, Copy)]
pub struct LogQueues<Q> {
    inbox: Q,
}

impl<Q> LogQueues<Q> {
    pub const fn new(inbox: Q) -> Self {
        Self { inbox }
    }
}

impl<ReplyTo, const MAX: usize, Q> LogReceiverState<ReplyTo, MAX> for LogQueues<Q>
where
    Q: AsyncEnqueue<LogEvent<ReplyTo, MAX>> + Clone,
{
    type InboxQueue = Q;

    fn log_inbox_queue(&self) -> Self::InboxQueue {
        self.inbox.clone()
    }
}

/// Convenience sender-side state wrapper.
#[derive(Debug, Clone, Copy)]
pub struct LogOutbox<Q> {
    outbox: Q,
}

impl<Q> LogOutbox<Q> {
    pub const fn new(outbox: Q) -> Self {
        Self { outbox }
    }
}

impl<ReplyTo, const MAX: usize, Q> LogSenderState<ReplyTo, MAX> for LogOutbox<Q>
where
    Q: AsyncDequeue<OutboundLog<ReplyTo, MAX>> + Clone,
{
    type OutboxQueue = Q;

    fn log_outbox_queue(&self) -> Self::OutboxQueue {
        self.outbox.clone()
    }
}

#[cfg(feature = "embassy")]
pub type LogInboxQueue<M, ReplyTo, const MAX: usize, const DEPTH: usize> =
    embassy_sync::channel::Channel<M, LogEvent<ReplyTo, MAX>, DEPTH>;

#[cfg(feature = "embassy")]
pub type LogOutboxQueue<M, ReplyTo, const MAX: usize, const DEPTH: usize> =
    embassy_sync::channel::Channel<M, OutboundLog<ReplyTo, MAX>, DEPTH>;

/// Embassy resources owned by the receiver-side log bundle.
///
/// This keeps inbox queue allocation inside the bundle crate so applications can hold one typed
/// resource object instead of declaring queue statics directly.
#[cfg(feature = "embassy")]
pub struct ReceiverBundleResources<M, ReplyTo, const MAX: usize, const DEPTH: usize>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
    ReplyTo: Copy,
{
    inbox: LogInboxQueue<M, ReplyTo, MAX, DEPTH>,
}

#[cfg(feature = "embassy")]
impl<M, ReplyTo, const MAX: usize, const DEPTH: usize>
    ReceiverBundleResources<M, ReplyTo, MAX, DEPTH>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
    ReplyTo: Copy,
{
    pub const fn new() -> Self {
        Self {
            inbox: embassy_sync::channel::Channel::new(),
        }
    }

    pub fn inbox_queue(&self) -> &LogInboxQueue<M, ReplyTo, MAX, DEPTH> {
        &self.inbox
    }

    pub fn receiver_state(&self) -> LogQueues<&LogInboxQueue<M, ReplyTo, MAX, DEPTH>> {
        LogQueues::new(self.inbox_queue())
    }
}

/// Embassy resources owned by the sender-side log bundle.
///
/// This keeps outbox queue allocation inside the bundle crate so applications can hold one typed
/// resource object instead of declaring queue statics directly.
#[cfg(feature = "embassy")]
pub struct SenderBundleResources<M, ReplyTo, const MAX: usize, const DEPTH: usize>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
    ReplyTo: Copy,
{
    outbox: LogOutboxQueue<M, ReplyTo, MAX, DEPTH>,
}

#[cfg(feature = "embassy")]
impl<M, ReplyTo, const MAX: usize, const DEPTH: usize>
    SenderBundleResources<M, ReplyTo, MAX, DEPTH>
where
    M: embassy_sync::blocking_mutex::raw::RawMutex,
    ReplyTo: Copy,
{
    pub const fn new() -> Self {
        Self {
            outbox: embassy_sync::channel::Channel::new(),
        }
    }

    pub fn outbox_queue(&self) -> &LogOutboxQueue<M, ReplyTo, MAX, DEPTH> {
        &self.outbox
    }

    pub fn sender_state(&self) -> LogOutbox<&LogOutboxQueue<M, ReplyTo, MAX, DEPTH>> {
        LogOutbox::new(self.outbox_queue())
    }
}

/// Helper to send log messages (embassy-style).
#[cfg(feature = "embassy")]
pub struct LogSender<'a, A: Atlas> {
    scratch: thincan::capnp::CapnpScratchRef<'a>,
    send_timeout: Duration,
    _atlas: PhantomData<A>,
}

#[cfg(feature = "embassy")]
impl<'a, A: Atlas> LogSender<'a, A> {
    pub fn new(scratch: thincan::capnp::CapnpScratchRef<'a>) -> Self {
        Self {
            scratch,
            send_timeout: Duration::from_millis(200),
            _atlas: PhantomData,
        }
    }

    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    pub async fn send<Node, Router, TxBuf, ReplyTo>(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
        to: ReplyTo,
        text: &str,
    ) -> Result<(), thincan::Error>
    where
        Node: can_isotp_interface::IsoTpAsyncEndpointMeta<ReplyTo = ReplyTo>,
        TxBuf: AsMut<[u8]>,
    {
        if text.as_bytes().len() > MAX_LOG_BYTES {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }

        let value = self
            .scratch
            .builder_value::<A::LogMsg, schema::log_msg::Owned, _>(|root| {
                root.set_text(text);
                Ok(())
            });
        iface
            .send_encoded_to_async(to, &value, self.send_timeout)
            .await
    }
}

/// Outbound participant that drains queued logs and transmits them.
#[cfg(feature = "embassy")]
pub struct LogBusParticipant<'a, A: Atlas, Q, const MAX: usize> {
    sender: LogSender<'a, A>,
    outbox: Q,
}

#[cfg(feature = "embassy")]
impl<'a, A, Q, const MAX: usize> LogBusParticipant<'a, A, Q, MAX>
where
    A: Atlas,
{
    pub fn new(
        outbox: Q,
        scratch: thincan::capnp::CapnpScratchRef<'a>,
        send_timeout: Duration,
    ) -> Self {
        let mut sender = LogSender::<A>::new(scratch);
        sender.set_send_timeout(send_timeout);
        Self { sender, outbox }
    }
}

#[cfg(feature = "embassy")]
impl<Node, Router, TxBuf, A, Q, const MAX: usize>
    thincan::BusParticipant<thincan::Interface<Node, Router, TxBuf>>
    for LogBusParticipant<'_, A, Q, MAX>
where
    A: Atlas,
    Node: can_isotp_interface::IsoTpAsyncEndpointMeta,
    TxBuf: AsMut<[u8]>,
    <Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo: Copy,
    Q: AsyncDequeue<
        OutboundLog<<Node as can_isotp_interface::IsoTpAsyncEndpointMeta>::ReplyTo, MAX>,
    >,
{
    async fn drain_outbound(
        &mut self,
        iface: &mut thincan::Interface<Node, Router, TxBuf>,
    ) -> Result<(), thincan::Error> {
        while let Ok(log) = self.outbox.try_recv() {
            let text = core::str::from_utf8(log.text_bytes()).unwrap_or("<non-utf8>");
            let _ = self.sender.send(iface, log.reply_to, text).await;
        }
        Ok(())
    }
}

/// Define a `thincan` bundle for the `LogMsg` message in your atlas.
#[macro_export]
macro_rules! thincan_log_bundle {
    ($vis:vis mod $bundle:ident ($atlas:ident) $( { $($tt:tt)* } )? ) => {
        ::thincan::bundle! {
            $vis mod $bundle($atlas) {
                parser: ::thincan::DefaultParser;
                use msgs [LogMsg];
                handles {
                    LogMsg => on_log,
                }
                items {
                    #[allow(dead_code)]
                    struct _AssertAtlas
                    where
                        super::$atlas::Atlas: $crate::Atlas;

                    #[derive(Clone)]
                    pub struct LogInboxHandlers<Q, ReplyTo, const MAX: usize> {
                        inbox: Q,
                        _reply_to: core::marker::PhantomData<ReplyTo>,
                    }

                    impl<Q, ReplyTo, const MAX: usize> LogInboxHandlers<Q, ReplyTo, MAX>
                    where
                        Q: $crate::AsyncEnqueue<$crate::LogEvent<ReplyTo, MAX>>,
                        ReplyTo: Copy,
                    {
                        pub fn new(inbox: Q) -> Self {
                            Self {
                                inbox,
                                _reply_to: core::marker::PhantomData,
                            }
                        }

                        pub fn from_app<S>(app: &S) -> Self
                        where
                            S: $crate::LogReceiverState<ReplyTo, MAX, InboxQueue = Q>,
                        {
                            Self::new(app.log_inbox_queue())
                        }
                    }

                    impl<'a, Q, ReplyTo, const MAX: usize> Handlers<'a, ReplyTo>
                        for LogInboxHandlers<Q, ReplyTo, MAX>
                    where
                        Q: $crate::AsyncEnqueue<$crate::LogEvent<ReplyTo, MAX>>,
                        ReplyTo: Copy,
                    {
                        type Error = core::convert::Infallible;

                        async fn on_log(
                            &mut self,
                            meta: ::thincan::RecvMeta<ReplyTo>,
                            msg: ::thincan::CapnpTyped<'a, $crate::schema::log_msg::Owned>,
                        ) -> Result<(), Self::Error> {
                            use core::cmp::min;
                            let mut data = [0u8; MAX];

                            let parsed = msg.with_root($crate::capnp::message::ReaderOptions::default(), |root| {
                                let text = root
                                    .get_text()
                                    .unwrap_or_else(|_| $crate::capnp::text::Reader::from(""));
                                let bytes = text.as_bytes();
                                let overflow = bytes.len() > MAX;
                                let copy_len = min(bytes.len(), MAX);
                                data[..copy_len].copy_from_slice(&bytes[..copy_len]);
                                (bytes.len(), overflow)
                            });

                            let (len, overflow) = parsed.unwrap_or((0, true));
                            let ev = $crate::LogEvent {
                                reply_to: meta.reply_to,
                                len: min(len, u16::MAX as usize) as u16,
                                data,
                                overflow,
                            };
                            self.inbox.send(ev).await;
                            Ok(())
                        }
                    }

                    impl<Iface, Q, ReplyTo, const MAX: usize> ::thincan::BundleOutbound<Iface>
                        for LogInboxHandlers<Q, ReplyTo, MAX>
                    where
                        Q: $crate::AsyncEnqueue<$crate::LogEvent<ReplyTo, MAX>>,
                        ReplyTo: Copy,
                    {
                        type Outbound = ();

                        fn take_outbound(&mut self) -> Self::Outbound {}
                    }

                    #[derive(Clone, Copy)]
                    pub struct NoopHandlers;

                    impl NoopHandlers {
                        pub const fn new() -> Self {
                            Self
                        }
                    }

                    impl<'a, ReplyTo> Handlers<'a, ReplyTo> for NoopHandlers
                    where
                        ReplyTo: Copy,
                    {
                        type Error = core::convert::Infallible;

                        async fn on_log(
                            &mut self,
                            _meta: ::thincan::RecvMeta<ReplyTo>,
                            _msg: ::thincan::CapnpTyped<'a, $crate::schema::log_msg::Owned>,
                        ) -> Result<(), Self::Error> {
                            Ok(())
                        }
                    }

                    impl<Iface> ::thincan::BundleOutbound<Iface> for NoopHandlers {
                        type Outbound = ();

                        fn take_outbound(&mut self) -> Self::Outbound {}
                    }

                    #[cfg(feature = "embassy")]
                    pub struct NoopHandlersWithOutbox<'a, Q, const MAX: usize> {
                        outbox: Option<$crate::LogBusParticipant<'a, super::$atlas::Atlas, Q, MAX>>,
                    }

                    #[cfg(feature = "embassy")]
                    impl<'a, Q, const MAX: usize> NoopHandlersWithOutbox<'a, Q, MAX> {
                        pub fn new(
                            outbox: $crate::LogBusParticipant<'a, super::$atlas::Atlas, Q, MAX>,
                        ) -> Self {
                            Self {
                                outbox: Some(outbox),
                            }
                        }
                    }

                    #[cfg(feature = "embassy")]
                    impl<'msg, 'state, Q, ReplyTo, const MAX: usize> Handlers<'msg, ReplyTo>
                        for NoopHandlersWithOutbox<'state, Q, MAX>
                    where
                        ReplyTo: Copy,
                    {
                        type Error = core::convert::Infallible;

                        async fn on_log(
                            &mut self,
                            _meta: ::thincan::RecvMeta<ReplyTo>,
                            _msg: ::thincan::CapnpTyped<'msg, $crate::schema::log_msg::Owned>,
                        ) -> Result<(), Self::Error> {
                            Ok(())
                        }
                    }

                    #[cfg(feature = "embassy")]
                    impl<'state, Iface, Q, const MAX: usize> ::thincan::BundleOutbound<Iface>
                        for NoopHandlersWithOutbox<'state, Q, MAX>
                    {
                        type Outbound =
                            $crate::LogBusParticipant<'state, super::$atlas::Atlas, Q, MAX>;

                        fn take_outbound(&mut self) -> Self::Outbound {
                            self.outbox.take().expect(
                                "log outbound already taken; build a new maplet runtime",
                            )
                        }
                    }

                    #[cfg(feature = "embassy")]
                    impl Bundle {
                        pub fn sender<'a>(
                            scratch: ::thincan::capnp::CapnpScratchRef<'a>,
                            send_timeout: core::time::Duration,
                        ) -> $crate::LogSender<'a, super::$atlas::Atlas> {
                            let mut sender = $crate::LogSender::<super::$atlas::Atlas>::new(scratch);
                            sender.set_send_timeout(send_timeout);
                            sender
                        }

                        pub fn outbox_participant<'a, Q, ReplyTo, const MAX: usize>(
                            outbox: Q,
                            scratch: ::thincan::capnp::CapnpScratchRef<'a>,
                            send_timeout: core::time::Duration,
                        ) -> $crate::LogBusParticipant<'a, super::$atlas::Atlas, Q, MAX>
                        where
                            Q: $crate::AsyncDequeue<$crate::OutboundLog<ReplyTo, MAX>>,
                        {
                            let _ = core::marker::PhantomData::<ReplyTo>;
                            $crate::LogBusParticipant::<super::$atlas::Atlas, Q, MAX>::new(
                                outbox,
                                scratch,
                                send_timeout,
                            )
                        }

                        pub fn outbox_participant_from_app<
                            'a,
                            S,
                            Q,
                            ReplyTo,
                            const MAX: usize,
                        >(
                            app: &S,
                            scratch: ::thincan::capnp::CapnpScratchRef<'a>,
                            send_timeout: core::time::Duration,
                        ) -> $crate::LogBusParticipant<'a, super::$atlas::Atlas, Q, MAX>
                        where
                            S: $crate::LogSenderState<ReplyTo, MAX, OutboxQueue = Q>,
                            Q: $crate::AsyncDequeue<$crate::OutboundLog<ReplyTo, MAX>>,
                        {
                            Self::outbox_participant::<Q, ReplyTo, MAX>(
                                app.log_outbox_queue(),
                                scratch,
                                send_timeout,
                            )
                        }

                        pub fn noop_handlers_with_outbox<'a, Q, ReplyTo, const MAX: usize>(
                            outbox: Q,
                            scratch: ::thincan::capnp::CapnpScratchRef<'a>,
                            send_timeout: core::time::Duration,
                        ) -> NoopHandlersWithOutbox<'a, Q, MAX>
                        where
                            Q: $crate::AsyncDequeue<$crate::OutboundLog<ReplyTo, MAX>>,
                        {
                            NoopHandlersWithOutbox::new(Self::outbox_participant::<Q, ReplyTo, MAX>(
                                outbox,
                                scratch,
                                send_timeout,
                            ))
                        }

                        pub fn noop_handlers_with_outbox_from_app<
                            'a,
                            S,
                            Q,
                            ReplyTo,
                            const MAX: usize,
                        >(
                            app: &S,
                            scratch: ::thincan::capnp::CapnpScratchRef<'a>,
                            send_timeout: core::time::Duration,
                        ) -> NoopHandlersWithOutbox<'a, Q, MAX>
                        where
                            S: $crate::LogSenderState<ReplyTo, MAX, OutboxQueue = Q>,
                            Q: $crate::AsyncDequeue<$crate::OutboundLog<ReplyTo, MAX>>,
                        {
                            Self::noop_handlers_with_outbox::<Q, ReplyTo, MAX>(
                                app.log_outbox_queue(),
                                scratch,
                                send_timeout,
                            )
                        }
                    }
                }
            }
        }
    };
}
