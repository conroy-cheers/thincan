#![cfg(feature = "async")]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

#[derive(Debug)]
struct ChannelTransport {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    recv_calls: Arc<Mutex<usize>>,
}

impl ChannelTransport {
    fn pair() -> (Self, Self, Arc<Mutex<usize>>, Arc<Mutex<usize>>) {
        let (a_tx, b_rx) = mpsc::channel::<Vec<u8>>(16);
        let (b_tx, a_rx) = mpsc::channel::<Vec<u8>>(16);

        let a_calls = Arc::new(Mutex::new(0usize));
        let b_calls = Arc::new(Mutex::new(0usize));

        let a = Self {
            tx: a_tx,
            rx: a_rx,
            recv_calls: a_calls.clone(),
        };
        let b = Self {
            tx: b_tx,
            rx: b_rx,
            recv_calls: b_calls.clone(),
        };
        (a, b, a_calls, b_calls)
    }
}

impl thincan::AsyncTransport for ChannelTransport {
    type SendFuture<'a>
        = Pin<Box<dyn core::future::Future<Output = Result<(), thincan::Error>> + Send + 'a>>
    where
        Self: 'a;

    type RecvFuture<'a>
        = Pin<
        Box<dyn core::future::Future<Output = Result<Vec<Vec<u8>>, thincan::Error>> + Send + 'a>,
    >
    where
        Self: 'a;

    fn send<'a>(&'a mut self, payload: &'a [u8], timeout: Duration) -> Self::SendFuture<'a> {
        let tx = self.tx.clone();
        let bytes = payload.to_vec();
        Box::pin(async move {
            tokio::time::timeout(timeout, tx.send(bytes))
                .await
                .map_err(|_| thincan::Error::timeout())?
                .map_err(|_e| thincan::Error {
                    kind: thincan::ErrorKind::Other,
                })?;
            Ok(())
        })
    }

    fn recv<'a>(&'a mut self, timeout: Duration) -> Self::RecvFuture<'a> {
        let recv_calls = self.recv_calls.clone();
        let rx = &mut self.rx;
        Box::pin(async move {
            *recv_calls.lock().unwrap() += 1;

            let first = tokio::time::timeout(timeout, rx.recv())
                .await
                .map_err(|_| thincan::Error::timeout())?
                .ok_or(thincan::Error::timeout())?;

            let mut out = vec![first];
            while let Ok(next) = rx.try_recv() {
                out.push(next);
            }
            Ok(out)
        })
    }
}

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => A(len = 1);
        0x0101 => B(len = 1);
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

    fn on_a(&mut self, msg: &'a [u8; atlas::A::BODY_LEN]) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xAA]);
        self.seen.push(<atlas::A as thincan::Message>::ID);
        Ok(())
    }

    fn on_b(&mut self, msg: &'a [u8; atlas::B::BODY_LEN]) -> Result<(), Self::Error> {
        assert_eq!(*msg, [0xBB]);
        self.seen.push(<atlas::B as thincan::Message>::ID);
        Ok(())
    }
}

#[tokio::test]
async fn async_interface_batches_multiple_rx() -> Result<(), thincan::Error> {
    let (a, b, a_calls, b_calls) = ChannelTransport::pair();

    let mut iface_a = thincan::AsyncInterface::new(a, maplet::Router::new());
    let mut iface_b = thincan::AsyncInterface::new(b, maplet::Router::new());

    iface_a
        .send_msg::<atlas::A>(&[0xAA], Duration::from_millis(50))
        .await?;
    iface_a
        .send_msg::<atlas::B>(&[0xBB], Duration::from_millis(50))
        .await?;

    let mut handlers = maplet::HandlersImpl {
        app: App::default(),
        none: none::DefaultHandlers,
    };

    assert_eq!(
        iface_b
            .recv_dispatch(&mut handlers, Duration::from_millis(50))
            .await?,
        thincan::DispatchOutcome::Handled
    );
    assert_eq!(
        iface_b
            .recv_dispatch(&mut handlers, Duration::from_millis(50))
            .await?,
        thincan::DispatchOutcome::Handled
    );

    assert_eq!(
        handlers.app.seen,
        vec![
            <atlas::A as thincan::Message>::ID,
            <atlas::B as thincan::Message>::ID
        ]
    );

    assert_eq!(*a_calls.lock().unwrap(), 0);
    assert_eq!(*b_calls.lock().unwrap(), 1);
    Ok(())
}
