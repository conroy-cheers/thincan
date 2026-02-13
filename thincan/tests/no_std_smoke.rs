#![cfg(not(feature = "std"))]
#![no_std]

// The test harness requires `std`, but we want this crate to be `#![no_std]` to mimic downstream.
extern crate std;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

fn block_on<F: Future>(mut fut: F) -> F::Output {
    // A tiny single-threaded executor for tests (no timers).
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on the stack for the duration of this function.
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

thincan::bus_atlas! {
    pub mod atlas {
        0x0100 => Ping(len = 1);
        0x0101 => Pong(len = 2);
    }
}

thincan::bundle! {
    pub mod demo_bundle(atlas) {
        parser: thincan::DefaultParser;
        use msgs [Ping, Pong];
        handles { Ping => on_ping }
        items {}
    }
}

thincan::bus_maplet! {
    pub mod demo_maplet: atlas {
        bundles [demo_bundle];
        parser: thincan::DefaultParser;
        use msgs [Ping, Pong];
        handles { Pong => on_pong }
        unhandled_by_default = true;
        ignore [];
    }
}

#[derive(Default)]
pub struct Handlers;

impl<'a> demo_bundle::Handlers<'a, demo_maplet::ReplyTo> for Handlers {
    type Error = ();
    async fn on_ping(
        &mut self,
        _meta: thincan::RecvMeta<demo_maplet::ReplyTo>,
        _msg: &'a [u8; atlas::Ping::BODY_LEN],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'a> demo_maplet::Handlers<'a> for Handlers {
    type Error = ();
    async fn on_pong(
        &mut self,
        _meta: thincan::RecvMeta<demo_maplet::ReplyTo>,
        _msg: &'a [u8; atlas::Pong::BODY_LEN],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn no_std_smoke_compiles_and_links() {
    use thincan::RouterDispatch;

    let mut router = demo_maplet::Router::new();
    let mut handlers = demo_maplet::HandlersImpl {
        app: Handlers::default(),
        demo_bundle: Handlers::default(),
    };

    let meta = thincan::RecvMeta { reply_to: () };
    let _ = block_on(router.dispatch(
        &mut handlers,
        meta,
        thincan::RawMessage {
            id: <atlas::Ping as thincan::Message>::ID,
            body: &[0u8; atlas::Ping::BODY_LEN],
        },
    ));
}
