#![cfg(feature = "std")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, IsoTpEndpoint, RecvControl, RecvError,
    RecvIntoStatus, RecvStatus, SendError,
};

#[derive(Default)]
struct SharedPipe {
    a_to_b: VecDeque<Vec<u8>>,
    b_to_a: VecDeque<Vec<u8>>,
}

#[derive(Clone)]
struct PipeEnd {
    shared: Arc<Mutex<SharedPipe>>,
    dir: Direction,
    recv_calls: Arc<Mutex<usize>>,
}

#[derive(Clone, Copy)]
enum Direction {
    A,
    B,
}

impl PipeEnd {
    fn pair() -> (Self, Self, Arc<Mutex<usize>>, Arc<Mutex<usize>>) {
        let shared = Arc::new(Mutex::new(SharedPipe::default()));
        let a_calls = Arc::new(Mutex::new(0usize));
        let b_calls = Arc::new(Mutex::new(0usize));
        let a = Self {
            shared: shared.clone(),
            dir: Direction::A,
            recv_calls: a_calls.clone(),
        };
        let b = Self {
            shared,
            dir: Direction::B,
            recv_calls: b_calls.clone(),
        };
        (a, b, a_calls, b_calls)
    }
}

impl IsoTpEndpoint for PipeEnd {
    type Error = thincan::Error;

    fn send(&mut self, payload: &[u8], _timeout: Duration) -> Result<(), SendError<Self::Error>> {
        let mut shared = self.shared.lock().unwrap();
        match self.dir {
            Direction::A => shared.a_to_b.push_back(payload.to_vec()),
            Direction::B => shared.b_to_a.push_back(payload.to_vec()),
        }
        Ok(())
    }

    fn recv_one<F>(
        &mut self,
        _timeout: Duration,
        mut on_payload: F,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        F: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        *self.recv_calls.lock().unwrap() += 1;

        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };

        if queue.is_empty() {
            return Ok(RecvStatus::TimedOut);
        }

        let payload = queue.pop_front().unwrap();
        let _ = on_payload(&payload).map_err(RecvError::Backend)?;
        Ok(RecvStatus::DeliveredOne)
    }
}

impl IsoTpAsyncEndpoint for PipeEnd {
    type Error = thincan::Error;

    async fn send(
        &mut self,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), SendError<Self::Error>> {
        IsoTpEndpoint::send(self, payload, timeout)
    }

    async fn recv_one<Cb>(
        &mut self,
        timeout: Duration,
        mut on_payload: Cb,
    ) -> Result<RecvStatus, RecvError<Self::Error>>
    where
        Cb: FnMut(&[u8]) -> Result<RecvControl, Self::Error>,
    {
        // Mirror the sync `IsoTpEndpoint` behavior for convenience in async tests.
        let _ = timeout;
        *self.recv_calls.lock().unwrap() += 1;

        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };

        if queue.is_empty() {
            return Ok(RecvStatus::TimedOut);
        }

        let payload = queue.pop_front().unwrap();
        let _ = on_payload(&payload).map_err(RecvError::Backend)?;
        Ok(RecvStatus::DeliveredOne)
    }
}

impl IsoTpAsyncEndpointRecvInto for PipeEnd {
    type Error = thincan::Error;

    async fn recv_one_into(
        &mut self,
        _timeout: Duration,
        out: &mut [u8],
    ) -> Result<RecvIntoStatus, RecvError<Self::Error>> {
        *self.recv_calls.lock().unwrap() += 1;

        let mut shared = self.shared.lock().unwrap();
        let queue = match self.dir {
            Direction::A => &mut shared.b_to_a,
            Direction::B => &mut shared.a_to_b,
        };

        if queue.is_empty() {
            return Ok(RecvIntoStatus::TimedOut);
        }

        let payload = queue.pop_front().unwrap();
        if out.len() < payload.len() {
            return Err(RecvError::BufferTooSmall {
                needed: payload.len(),
                got: out.len(),
            });
        }
        out[..payload.len()].copy_from_slice(&payload);
        Ok(RecvIntoStatus::DeliveredOne { len: payload.len() })
    }
}

thincan::bus_atlas! {
    pub mod atlas {
        0x0010 => A(len = 1);
        0x0011 => B(len = 1);
    }
}

thincan::bundle! {
    pub mod none(atlas) {
        parser: thincan::DefaultParser;
        use msgs [];
        handles {}
        items {}
    }
}

thincan::bus_maplet! {
    pub mod maplet: atlas {
        bundles [none];
        parser: thincan::DefaultParser;
        use msgs [A, B];
        handles { A => on_a, B => on_b }
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct App {
    seen: Vec<u16>,
}

impl<'a> maplet::Handlers<'a> for App {
    type Error = ();

    async fn on_a(
        &mut self,
        _meta: thincan::RecvMeta<maplet::ReplyTo>,
        msg: &'a [u8; atlas::A::BODY_LEN],
    ) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xAA]);
        self.seen.push(<atlas::A as thincan::Message>::ID);
        Ok(())
    }

    async fn on_b(
        &mut self,
        _meta: thincan::RecvMeta<maplet::ReplyTo>,
        msg: &'a [u8; atlas::B::BODY_LEN],
    ) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xBB]);
        self.seen.push(<atlas::B as thincan::Message>::ID);
        Ok(())
    }
}

#[test]
fn send_side_validation_rejects_wrong_len() {
    let (a, _b, _a_calls, _b_calls) = PipeEnd::pair();
    let mut tx = [0u8; 64];
    let mut iface = thincan::Interface::new(a, maplet::Router::new(), &mut tx);

    let err = iface
        .send_msg::<atlas::A>(&[0xAA, 0xBB], Duration::from_millis(1))
        .unwrap_err();
    assert_eq!(
        err.kind,
        thincan::ErrorKind::InvalidBodyLen {
            expected: atlas::A::BODY_LEN,
            got: 2
        }
    );
}

#[tokio::test]
async fn recv_one_dispatch_requires_one_transport_recv_per_payload() -> Result<(), thincan::Error>
{
    let (a, b, a_calls, b_calls) = PipeEnd::pair();

    let mut tx_a = [0u8; 64];
    let mut tx_b = [0u8; 64];
    let mut iface_a = thincan::Interface::new(a, maplet::Router::new(), &mut tx_a);
    let mut iface_b = thincan::Interface::new(b, maplet::Router::new(), &mut tx_b);

    iface_a.send_msg::<atlas::A>(&[0xAA], Duration::from_millis(1))?;
    iface_a.send_msg::<atlas::B>(&[0xBB], Duration::from_millis(1))?;

    let mut handlers = maplet::HandlersImpl {
        app: App::default(),
        none: none::DefaultHandlers,
    };
    let mut rx = [0u8; 64];

    assert_eq!(
        iface_b
            .recv_one_dispatch_async(&mut handlers, Duration::from_millis(1), &mut rx)
            .await?,
        thincan::RecvDispatch::Dispatched {
            id: <atlas::A as thincan::Message>::ID,
            kind: thincan::RecvDispatchKind::Handled
        }
    );
    assert_eq!(
        iface_b
            .recv_one_dispatch_async(&mut handlers, Duration::from_millis(1), &mut rx)
            .await?,
        thincan::RecvDispatch::Dispatched {
            id: <atlas::B as thincan::Message>::ID,
            kind: thincan::RecvDispatchKind::Handled
        }
    );

    assert_eq!(
        handlers.app.seen,
        vec![
            <atlas::A as thincan::Message>::ID,
            <atlas::B as thincan::Message>::ID
        ]
    );

    // Critical: without buffering, one dispatched message requires one endpoint `recv_one`.
    assert_eq!(*a_calls.lock().unwrap(), 0);
    assert_eq!(*b_calls.lock().unwrap(), 2);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AValue(u8);

impl thincan::Encode<atlas::A> for AValue {
    fn max_encoded_len(&self) -> usize {
        1
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        if out.is_empty() {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        out[0] = self.0;
        Ok(1)
    }
}

#[tokio::test]
async fn send_encoded_writes_and_validates() -> Result<(), thincan::Error> {
    let (a, b, _a_calls, _b_calls) = PipeEnd::pair();

    let mut tx_a = [0u8; 64];
    let mut tx_b = [0u8; 64];
    let mut iface_a = thincan::Interface::new(a, maplet::Router::new(), &mut tx_a);
    let mut iface_b = thincan::Interface::new(b, maplet::Router::new(), &mut tx_b);

    iface_a.send_encoded::<atlas::A, _>(&AValue(0xAA), Duration::from_millis(1))?;

    let mut handlers = maplet::HandlersImpl {
        app: App::default(),
        none: none::DefaultHandlers,
    };

    let mut rx = [0u8; 64];
    let _ = iface_b
        .recv_one_dispatch_async(&mut handlers, Duration::from_millis(1), &mut rx)
        .await?;
    assert_eq!(handlers.app.seen, vec![<atlas::A as thincan::Message>::ID]);
    Ok(())
}
