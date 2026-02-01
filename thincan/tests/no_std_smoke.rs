#![cfg(not(feature = "std"))]
#![no_std]

// The test harness requires `std`, but we want this crate to be `#![no_std]` to mimic downstream.
extern crate std;

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

impl<'a> demo_bundle::Handlers<'a> for Handlers {
    type Error = ();
    fn on_ping(&mut self, _msg: &'a [u8; atlas::Ping::BODY_LEN]) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'a> demo_maplet::Handlers<'a> for Handlers {
    type Error = ();
    fn on_pong(&mut self, _msg: &'a [u8; atlas::Pong::BODY_LEN]) -> Result<(), Self::Error> {
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

    let _ = router.dispatch(
        &mut handlers,
        thincan::RawMessage {
            id: <atlas::Ping as thincan::Message>::ID,
            body: &[0u8; atlas::Ping::BODY_LEN],
        },
    );
}
