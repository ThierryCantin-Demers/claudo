//! Checks that every WebSocket is a client of one shared backend: a second socket hears
//! what the first one's command changed, but not the answer meant for the first alone, and
//! each hears the control room on arrival.
//! Runs no turn. Start the daemon with `CLAUDO_TOKEN=test`, then:
//!   cargo run -p claudo-daemon --example hub_probe

use std::process::ExitCode;
use std::time::Duration;

use claudo_protocol::{Command, RoomEvent, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> ExitCode {
    let token = std::env::var("CLAUDO_TOKEN").unwrap_or_else(|_| "test".to_string());
    let port = std::env::var("CLAUDO_PORT").unwrap_or_else(|_| "8787".to_string());
    let url = format!("ws://127.0.0.1:{port}/ws?token={token}");
    let (asker, _) = connect_async(&url).await.expect("connect the first socket");
    let (watcher, _) = connect_async(&url)
        .await
        .expect("connect the second socket");
    let (mut send, mut asked) = asker.split();
    let (_, mut watched) = watcher.split();

    for command in [Command::GetStatus, Command::ListConversations] {
        let json = serde_json::to_string(&command).unwrap();
        send.send(Message::Text(json.into())).await.unwrap();
    }
    let (asked, watched) = tokio::join!(kinds(&mut asked), kinds(&mut watched));
    println!("first socket heard:  {asked:?}");
    println!("second socket heard: {watched:?}");

    let shared = watched.contains(&"Conversations") && !watched.contains(&"Status");
    let answered = asked.contains(&"Status") && asked.contains(&"Conversations");
    let room = asked.contains(&"Room snapshot") && watched.contains(&"Room snapshot");
    if shared && answered && room {
        println!("ok: one backend, answers to the asker, news to all");
        ExitCode::SUCCESS
    } else {
        println!("FAILED");
        ExitCode::FAILURE
    }
}

/// The kind of each event a socket hears within two seconds.
async fn kinds<S>(socket: &mut S) -> Vec<&'static str>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut heard = Vec::new();
    let window = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(window);
    loop {
        tokio::select! {
            () = &mut window => return heard,
            message = socket.next() => {
                let Some(Ok(Message::Text(text))) = message else { return heard };
                if let Ok(event) = serde_json::from_str::<ServerEvent>(&text) {
                    heard.push(match event {
                        ServerEvent::Status(_) => "Status",
                        ServerEvent::Conversations { .. } => "Conversations",
                        ServerEvent::Room(RoomEvent::Snapshot(_)) => "Room snapshot",
                        _ => "other",
                    });
                }
            }
        }
    }
}
