//! Run the test mock of `org.kde.kdeconnect` on the session bus until killed,
//! to try `phonelink-rs --dump` (and later the UI) without a phone:
//! `cargo run --example mock_daemon & cargo run -- --dump`.

#[path = "../tests/support/mock.rs"]
mod mock;

use std::rc::Rc;

fn main() {
    let ctx = glib::MainContext::default();
    ctx.block_on(async {
        let conn = gio::bus_get_future(gio::BusType::Session).await.expect("session bus");
        mock::register_mock(&conn, Rc::new(mock::Seen::default()));
        mock::own_name(&conn).await;
        eprintln!("mock org.kde.kdeconnect running (device {})", mock::DEVICE);
        std::future::pending::<()>().await;
    });
}
