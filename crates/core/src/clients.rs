//! The frontends one backend serves at once, such as the app's own window and claudo on
//! another computer: what happens goes to each, and an answer only to the one that asked.

use std::sync::{Arc, Mutex};

use claudo_protocol::ServerEvent;
use tokio::sync::mpsc::UnboundedSender;

pub type ClientId = u64;

/// The frontend the backend was made with.
pub const LOCAL: ClientId = 0;

type Client = (ClientId, UnboundedSender<ServerEvent>);

#[derive(Clone, Default)]
pub struct Clients(Arc<Mutex<Vec<Client>>>);

impl Clients {
    pub fn add(&self, id: ClientId, events: UnboundedSender<ServerEvent>) {
        self.0.lock().unwrap().push((id, events));
    }

    pub fn remove(&self, id: ClientId) {
        self.0.lock().unwrap().retain(|(client, _)| *client != id);
    }

    /// To every client.
    pub fn send(&self, event: ServerEvent) {
        self.send_where(event, |_| true);
    }

    pub fn to(&self, id: ClientId, event: ServerEvent) {
        self.send_where(event, |client| client == id);
    }

    /// To every client but `id`.
    pub fn others(&self, id: ClientId, event: ServerEvent) {
        self.send_where(event, |client| client != id);
    }

    fn send_where(&self, event: ServerEvent, to: impl Fn(ClientId) -> bool) {
        let clients = self.0.lock().unwrap();
        let mut chosen = clients.iter().filter(|(client, _)| to(*client)).peekable();
        while let Some((_, events)) = chosen.next() {
            if chosen.peek().is_none() {
                let _ = events.send(event);
                return;
            }
            let _ = events.send(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn an_answer_goes_to_the_client_that_asked_and_news_to_all() {
        let clients = Clients::default();
        let (local, mut local_events) = unbounded_channel();
        let (remote, mut remote_events) = unbounded_channel();
        clients.add(LOCAL, local);
        clients.add(1, remote);
        clients.to(1, ServerEvent::Quota(Default::default()));
        clients.send(ServerEvent::QuotaUnavailable("all".to_string()));
        clients.others(1, ServerEvent::QuotaUnavailable("others".to_string()));
        clients.remove(1);
        clients.send(ServerEvent::QuotaUnavailable("gone".to_string()));
        let drain = |events: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| {
            std::iter::from_fn(|| events.try_recv().ok())
                .map(|event| match event {
                    ServerEvent::Quota(_) => "answer".to_string(),
                    ServerEvent::QuotaUnavailable(what) => what,
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(drain(&mut local_events), ["all", "others", "gone"]);
        assert_eq!(drain(&mut remote_events), ["answer", "all"]);
    }
}
