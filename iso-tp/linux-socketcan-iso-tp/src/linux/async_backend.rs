//! Addressed readiness-driven sockets with deferred kernel close.
use super::*;
use can_isotp_interface::{
    Deadline, Destination, IsoTpAsyncReadSend, IsoTpAsyncWriteSend, ReceiveError, ReceivedPayload,
    until,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use tokio::{
    io::{Interest, Ready, unix::AsyncFd},
    sync::Notify,
};

/// Fixed route for raw CAN-ID bindings.
#[derive(Debug, Clone, Copy)]
pub struct SocketRoute {
    pub receive_from: u8,
    pub transmit_to: Destination,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Receive,
    Send,
}
#[derive(Debug)]
pub struct TerminalFailure {
    pub operation: Operation,
    pub error: Arc<Error>,
}
struct Status {
    /// The first fatal failure; set once, so checking it takes no lock.
    fatal: std::sync::OnceLock<Arc<TerminalFailure>>,
    changed: Notify,
    closed: AtomicBool,
}
impl Status {
    fn new() -> Self {
        Self {
            fatal: std::sync::OnceLock::new(),
            changed: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }
    fn fail(&self, operation: Operation, error: Error) -> Arc<Error> {
        // First failure wins; later ones return the original cause.
        let failure = self.fatal.get_or_init(|| {
            Arc::new(TerminalFailure {
                operation,
                error: Arc::new(error),
            })
        });
        let error = failure.error.clone();
        self.changed.notify_waiters();
        error
    }
    fn failed(&self) -> bool {
        self.fatal.get().is_some()
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    /// Wait until `done` holds, rechecking after every status change. Enabling
    /// the notification before checking means no change can slip between.
    async fn wait_until(&self, done: impl Fn(&Self) -> bool) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if done(self) {
                return;
            }
            changed.await;
        }
    }
    async fn invalidated(&self) {
        self.wait_until(|status| status.failed() || status.is_closed())
            .await;
    }
}
/// Observes the first fatal error and eventual kernel close; does not own the fd.
#[derive(Clone)]
pub struct SocketControl {
    status: Arc<Status>,
}
impl SocketControl {
    pub async fn failed(&self) -> Option<Arc<TerminalFailure>> {
        self.status.invalidated().await;
        self.status.fatal.get().cloned()
    }
    pub async fn closed(&self) {
        self.status.wait_until(Status::is_closed).await;
    }
    pub fn is_closed(&self) -> bool {
        self.status.is_closed()
    }
}

struct Permit {
    occupied: Arc<AtomicUsize>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.occupied.fetch_sub(1, Ordering::AcqRel);
    }
}
struct CloseJob {
    socket: SocketCanIsoTp,
    permit: Permit,
    status: Arc<Status>,
}
/// Bounded live/closing socket admission with dedicated standard close workers.
/// Dropping this handle never joins workers. Existing sockets retain submission
/// handles, and workers drain their queue after the last handle disappears.
#[derive(Clone)]
pub struct SocketCloser {
    jobs: mpsc::SyncSender<CloseJob>,
    occupied: Arc<AtomicUsize>,
    limit: usize,
}
impl SocketCloser {
    pub fn new(limit: usize, workers: usize) -> Result<Self, Error> {
        if limit == 0 || workers == 0 || workers > limit {
            return Err(Error::InvalidConfig(
                "close capacity and worker count must be nonzero; workers <= capacity",
            ));
        }
        let (jobs, receiver) = mpsc::sync_channel::<CloseJob>(limit);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("isotp-close-{index}"))
                .spawn(move || {
                    loop {
                        let job = receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recv();
                        let Ok(job) = job else {
                            break;
                        };
                        // Linux isotp_release can wait for an accepted PDU here.
                        drop(job.socket);
                        drop(job.permit);
                        job.status.closed.store(true, Ordering::Release);
                        job.status.changed.notify_waiters();
                    }
                })
                .map_err(Error::Io)?;
        }
        Ok(Self {
            jobs,
            occupied: Arc::new(AtomicUsize::new(0)),
            limit,
        })
    }
    pub fn occupied(&self) -> usize {
        self.occupied.load(Ordering::Acquire)
    }
    fn acquire(&self) -> Result<Permit, Error> {
        let mut occupied = self.occupied.load(Ordering::Acquire);
        loop {
            if occupied >= self.limit {
                return Err(Error::CloseCapacity);
            }
            match self.occupied.compare_exchange_weak(
                occupied,
                occupied + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => occupied = actual,
            }
        }
        Ok(Permit {
            occupied: self.occupied.clone(),
        })
    }
}
struct DeferredSocket {
    socket: Option<SocketCanIsoTp>,
    permit: Option<Permit>,
    jobs: mpsc::SyncSender<CloseJob>,
    status: Arc<Status>,
}
impl AsRawFd for DeferredSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_ref().expect("registered socket").as_raw_fd()
    }
}
impl Drop for DeferredSocket {
    fn drop(&mut self) {
        let job = CloseJob {
            socket: self.socket.take().expect("socket owned until drop"),
            permit: self.permit.take().expect("close permit"),
            status: self.status.clone(),
        };
        // Each submitting socket owns a permit; queued jobs + worker jobs +
        // live sockets <= capacity. Therefore the reserved slot cannot be full.
        self.jobs
            .try_send(job)
            .unwrap_or_else(|_| panic!("socket closer admission invariant violated"));
    }
}
struct Shared {
    io: AsyncFd<DeferredSocket>,
    status: Arc<Status>,
    route: SocketRoute,
}
/// Tokio socket configured for one addressed route.
pub struct TokioSocketCanIsoTp {
    shared: Arc<Shared>,
}
/// Exclusively owned receive half.
pub struct TokioIsoTpReadHalf {
    shared: Arc<Shared>,
}
/// Exclusively owned transmit half.
pub struct TokioIsoTpWriteHalf {
    shared: Arc<Shared>,
}
pub struct SocketParts {
    pub reader: TokioIsoTpReadHalf,
    pub writer: TokioIsoTpWriteHalf,
    pub control: SocketControl,
}
impl TokioSocketCanIsoTp {
    pub fn open_peer(
        closer: &SocketCloser,
        iface: &str,
        local: u8,
        peer: u8,
        options: &IsoTpKernelOptions,
    ) -> Result<Self, Error> {
        if local == peer {
            return Err(Error::InvalidConfig("peer must differ from local address"));
        }
        Self::open_with_ids(
            closer,
            iface,
            uds_phys_id(local, peer),
            uds_phys_id(peer, local),
            SocketRoute {
                receive_from: peer,
                transmit_to: Destination::Physical(peer),
            },
            options,
        )
    }
    pub fn open_with_ids(
        closer: &SocketCloser,
        iface: &str,
        rx_id: Id,
        tx_id: Id,
        route: SocketRoute,
        options: &IsoTpKernelOptions,
    ) -> Result<Self, Error> {
        validate_options(options)?;
        if options.socket.flags & flags::CAN_ISOTP_WAIT_TX_DONE != 0 {
            return Err(Error::InvalidConfig(
                "CAN_ISOTP_WAIT_TX_DONE can block the executor",
            ));
        }
        let permit = closer.acquire()?;
        let status = Arc::new(Status::new());
        let socket = SocketCanIsoTp {
            fd: OwnedFd::from(new_isotp_socket()?),
            rx_buf: Vec::new(),
        };
        let deferred = DeferredSocket {
            socket: Some(socket),
            permit: Some(permit),
            jobs: closer.jobs.clone(),
            status: status.clone(),
        };
        // Configure through `deferred` so a setup failure still closes via the closer.
        let fd = &deferred.socket.as_ref().expect("owned socket").fd;
        configure_socket(socket2::SockRef::from(fd), iface, rx_id, tx_id, options)?;
        let io = AsyncFd::new(deferred).map_err(Error::Io)?;
        Ok(Self {
            shared: Arc::new(Shared { io, status, route }),
        })
    }
    pub fn into_split(self) -> SocketParts {
        let control = SocketControl {
            status: self.shared.status.clone(),
        };
        SocketParts {
            reader: TokioIsoTpReadHalf {
                shared: self.shared.clone(),
            },
            writer: TokioIsoTpWriteHalf {
                shared: self.shared,
            },
            control,
        }
    }
}

fn transaction_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ETIMEDOUT | libc::EILSEQ | libc::EBADMSG | libc::ECOMM | libc::EMSGSIZE)
    )
}
/// Send errors that refuse one request but leave the socket usable: transaction
/// errors, an invalid request (`EINVAL`), and a full CAN transmit queue
/// (`ENOBUFS`, e.g. a busy bus filling the qdisc).
fn send_rejection(error: &io::Error) -> bool {
    transaction_error(error) || matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOBUFS))
}
impl IsoTpAsyncReadSend for TokioIsoTpReadHalf {
    type Error = Arc<Error>;
    async fn recv_into(
        &mut self,
        out: &mut [u8],
    ) -> Result<ReceivedPayload, ReceiveError<Self::Error>> {
        let shared = &self.shared;
        loop {
            if shared.status.failed() {
                return Err(ReceiveError::Closed);
            }
            let mut guard = tokio::select! {biased;()=shared.status.invalidated()=>return Err(ReceiveError::Closed),ready=shared.io.ready(Interest::READABLE|Interest::ERROR)=>ready.map_err(|e|ReceiveError::Fatal(shared.status.fail(Operation::Receive,Error::Io(e))))?};
            let result = guard.try_io(|io| {
                // SAFETY: registered fd is retained and out is writable for its length.
                let read = unsafe {
                    libc::recv(
                        io.get_ref().as_raw_fd(),
                        out.as_mut_ptr().cast(),
                        out.len(),
                        libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                    )
                };
                if read < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(read as usize)
                }
            });
            match result {
                Ok(Ok(0)) => {
                    return Err(ReceiveError::Fatal(shared.status.fail(
                        Operation::Receive,
                        Error::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "ISO-TP socket returned zero bytes",
                        )),
                    )));
                }
                Ok(Ok(len)) if len > out.len() => {
                    return Err(ReceiveError::BufferTooSmall {
                        needed: len,
                        got: out.len(),
                    });
                }
                Ok(Ok(len)) => {
                    return Ok(ReceivedPayload {
                        meta: RecvMeta {
                            reply_to: shared.route.receive_from,
                        },
                        len,
                    });
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => {
                    guard.clear_ready_matching(Ready::ERROR);
                    return Err(if transaction_error(&e) {
                        ReceiveError::Rejected(Arc::new(Error::Io(e)))
                    } else {
                        ReceiveError::Fatal(shared.status.fail(Operation::Receive, Error::Io(e)))
                    });
                }
                Err(_) => continue,
            }
        }
    }
}
impl IsoTpAsyncWriteSend for TokioIsoTpWriteHalf {
    type Error = Arc<Error>;
    type Deadline = tokio::time::Instant;
    async fn send(
        &mut self,
        destination: Destination,
        payload: &[u8],
        deadline: Self::Deadline,
    ) -> Result<(), can_isotp_interface::TransmitError<Self::Error>> {
        use can_isotp_interface::TransmitError;
        let shared = &self.shared;
        if shared.status.failed() {
            return Err(TransmitError::Closed);
        }
        if destination != shared.route.transmit_to {
            return Err(TransmitError::Rejected(Arc::new(Error::WrongDestination {
                requested: destination,
                configured: shared.route.transmit_to,
            })));
        }
        let operation = async {
            loop {
                let mut guard = tokio::select! {
                    biased;
                    () = shared.status.invalidated() => return Err(TransmitError::Closed),
                    readiness = shared.io.ready(Interest::WRITABLE | Interest::ERROR) => {
                        readiness.map_err(|error| TransmitError::Fatal(shared.status.fail(Operation::Send, Error::Io(error))))?
                    }
                };
                let error_ready = guard.ready().is_error();
                match guard.try_io(|io| {
                    let fd = io.get_ref().as_raw_fd();
                    if error_ready && let Some(error) = take_socket_error(fd)? {
                        return Err(error);
                    }
                    // Check after all readiness/error work, directly before acceptance.
                    if deadline.remaining().is_zero() {
                        return Ok(None);
                    }
                    // SAFETY: payload remains borrowed and fd registered until return.
                    let sent = unsafe {
                        libc::send(
                            fd,
                            payload.as_ptr().cast(),
                            payload.len(),
                            libc::MSG_DONTWAIT,
                        )
                    };
                    if sent < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(Some(sent as usize))
                    }
                }) {
                    Ok(Ok(None)) => return Err(TransmitError::Timeout),
                    Ok(Ok(Some(len))) if len == payload.len() => return Ok(()),
                    Ok(Ok(Some(_))) => {
                        return Err(TransmitError::Fatal(shared.status.fail(
                            Operation::Send,
                            Error::Io(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "partial ISO-TP datagram send",
                            )),
                        )));
                    }
                    Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(error)) => {
                        guard.clear_ready_matching(Ready::ERROR);
                        return Err(if send_rejection(&error) {
                            TransmitError::Rejected(Arc::new(Error::Io(error)))
                        } else {
                            TransmitError::Fatal(
                                shared.status.fail(Operation::Send, Error::Io(error)),
                            )
                        });
                    }
                    Err(_) => continue,
                }
            }
        };
        until(deadline, operation)
            .await
            .unwrap_or(Err(TransmitError::Timeout))
    }
}
fn take_socket_error(fd: RawFd) -> io::Result<Option<io::Error>> {
    let mut error: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: both writable output pointers have the indicated size.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut error as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((error != 0).then(|| io::Error::from_raw_os_error(error)))
}

#[cfg(test)]
mod tests;
