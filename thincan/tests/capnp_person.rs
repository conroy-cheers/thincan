#![cfg(all(feature = "std", feature = "capnp"))]

#[path = "support/person_capnp.rs"]
mod person_capnp;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use can_isotp_interface::{
    IsoTpAsyncEndpoint, IsoTpAsyncEndpointRecvInto, IsoTpEndpoint, RecvControl, RecvError,
    RecvIntoStatus, RecvStatus, SendError,
};
use capnp::message::ReaderOptions;
use capnp::message::SingleSegmentAllocator;

#[derive(Default)]
struct SharedPipe {
    a_to_b: VecDeque<Vec<u8>>,
    b_to_a: VecDeque<Vec<u8>>,
}

#[derive(Clone)]
struct PipeEnd {
    shared: Arc<Mutex<SharedPipe>>,
    dir: Direction,
}

#[derive(Clone, Copy)]
enum Direction {
    A,
    B,
}

impl PipeEnd {
    fn pair() -> (Self, Self) {
        let shared = Arc::new(Mutex::new(SharedPipe::default()));
        let a = Self {
            shared: shared.clone(),
            dir: Direction::A,
        };
        let b = Self {
            shared,
            dir: Direction::B,
        };
        (a, b)
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
        let _ = timeout;
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
        0x2000 => Person(capnp = crate::person_capnp::person::Owned);
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
        use msgs [Person];
        handles { Person => on_person }
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
struct App {
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl<'a> maplet::Handlers<'a> for App {
    type Error = ();

    async fn on_person(
        &mut self,
        _meta: thincan::RecvMeta<maplet::ReplyTo>,
        msg: thincan::CapnpTyped<'a, person_capnp::person::Owned>,
    ) -> Result<(), Self::Error> {
        let (name, email) = msg
            .with_root(ReaderOptions::default(), |root| {
                (
                    root.get_name().unwrap().to_str().unwrap().to_owned(),
                    root.get_email().unwrap().to_str().unwrap().to_owned(),
                )
            })
            .unwrap();
        self.seen.lock().unwrap().push((name, email));
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonValue {
    name: &'static str,
    email: &'static str,
}

impl thincan::Encode<atlas::Person> for PersonValue {
    fn max_encoded_len(&self) -> usize {
        1024
    }

    fn encode(&self, out: &mut [u8]) -> Result<usize, thincan::Error> {
        // Build a single-segment capnp message in a scratch buffer, then send the raw segment bytes
        // (no framing), matching `thincan::Capnp`'s single-segment decode model.
        let mut builder_buf = [0u8; 1024];
        let mut message =
            capnp::message::Builder::new(SingleSegmentAllocator::new(&mut builder_buf));
        let mut person: person_capnp::person::Builder = message.init_root();
        person.set_name(self.name);
        person.set_email(self.email);

        let segments = message.get_segments_for_output();
        if segments.len() != 1 {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        let bytes = segments[0];
        if out.len() < bytes.len() {
            return Err(thincan::Error {
                kind: thincan::ErrorKind::Other,
            });
        }
        out[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }
}

#[tokio::test]
async fn capnp_person_can_roundtrip_through_interface() -> Result<(), thincan::Error> {
    let (a, b) = PipeEnd::pair();

    let mut tx_a = [0u8; 2048];
    let mut tx_b = [0u8; 2048];
    let mut iface_a = thincan::Interface::new(a, maplet::Router::new(), &mut tx_a);
    let mut iface_b = thincan::Interface::new(b, maplet::Router::new(), &mut tx_b);

    iface_a.send_encoded::<atlas::Person, _>(
        &PersonValue {
            name: "Bob Jones",
            email: "bob@example.com",
        },
        Duration::from_millis(1),
    )?;

    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let mut handlers = maplet::HandlersImpl {
        app: App { seen: seen.clone() },
        none: none::DefaultHandlers,
    };

    let mut rx = [0u8; 2048];
    let _ = iface_b
        .recv_one_dispatch_async(&mut handlers, Duration::from_millis(1), &mut rx)
        .await?;

    assert_eq!(
        *seen.lock().unwrap(),
        vec![("Bob Jones".to_string(), "bob@example.com".to_string())]
    );
    Ok(())
}
