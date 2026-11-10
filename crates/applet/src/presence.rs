//! Whether anyone is at this computer, from the compositor's idle notifications. A
//! computer used only over ssh sits idle, and its applets stop asking for usage.

use std::time::Duration;

use cosmic::cctk::wayland_client::globals::{GlobalListContents, registry_queue_init};
use cosmic::cctk::wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use cosmic::cctk::wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use cosmic::cctk::wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use cosmic::iced::futures::Stream;
use cosmic::iced::futures::channel::mpsc;

/// No input for this long and nobody is there.
const AWAY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether someone is at the seat, each time that changes. Silent where the compositor
/// offers no idle notifications, so the applet asks as if someone always were.
pub fn watch() -> impl Stream<Item = bool> {
    let (sender, present) = mpsc::unbounded();
    std::thread::spawn(move || {
        let Ok(connection) = Connection::connect_to_env() else {
            return;
        };
        let Ok((globals, mut queue)) = registry_queue_init::<Seat>(&connection) else {
            return;
        };
        let handle = queue.handle();
        let (Ok(notifier), Ok(seat)) = (
            globals.bind::<ExtIdleNotifierV1, _, _>(&handle, 1..=2, ()),
            globals.bind::<WlSeat, _, _>(&handle, 1..=1, ()),
        ) else {
            return;
        };
        let after = AWAY_AFTER.as_millis() as u32;
        // Version 2 goes by input alone, so a playing video does not count as someone there.
        let _notification = if notifier.version() >= 2 {
            notifier.get_input_idle_notification(after, &seat, &handle, ())
        } else {
            notifier.get_idle_notification(after, &seat, &handle, ())
        };
        let mut seat = Seat { sender };
        while queue.blocking_dispatch(&mut seat).is_ok() && !seat.sender.is_closed() {}
    });
    present
}

struct Seat {
    sender: mpsc::UnboundedSender<bool>,
}

impl Dispatch<ExtIdleNotificationV1, ()> for Seat {
    fn event(
        seat: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let present = match event {
            ext_idle_notification_v1::Event::Idled => false,
            ext_idle_notification_v1::Event::Resumed => true,
            _ => return,
        };
        let _ = seat.sender.unbounded_send(present);
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Seat {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Seat {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Seat {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
