//! Smoke test for the M4b WS daemon: connect, exchange protocol JSON, run a
//! turn. Start the daemon first with `CLAUDO_TOKEN=test`, then:
//!   cargo run -p claudo-daemon --example ws_probe

use claudo_protocol::{Command, Effort, Profile, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let token = std::env::var("CLAUDO_TOKEN").unwrap_or_else(|_| "test".to_string());
    let port = std::env::var("CLAUDO_PORT").unwrap_or_else(|_| "8787".to_string());
    let url = format!("ws://127.0.0.1:{port}/ws?token={token}");

    let (ws, _) = connect_async(&url).await.expect("connect to daemon");
    let (mut write, mut read) = ws.split();

    let json = |cmd: &Command| serde_json::to_string(cmd).unwrap();
    write
        .send(Message::Text(json(&Command::GetStatus).into()))
        .await
        .unwrap();
    write
        .send(Message::Text(
            json(&Command::NewScratch { request: 0 }).into(),
        ))
        .await
        .unwrap();

    let mut assistant = String::new();
    let mut sent = false;
    while let Some(Ok(msg)) = read.next().await {
        let Message::Text(txt) = msg else { continue };
        let Ok(ev) = serde_json::from_str::<ServerEvent>(&txt) else {
            continue;
        };
        match ev {
            ServerEvent::Status(s) => println!("Status: ready={}", s.ready()),
            ServerEvent::Conversations { list } => {
                println!("Conversations: {} item(s)", list.len())
            }
            ServerEvent::Created {
                conversation_id, ..
            } => {
                println!("Created: {conversation_id}");
                if !sent {
                    sent = true;
                    let cmd = Command::SendMessage {
                        conversation_id,
                        text: "Reply with a short greeting.".to_string(),
                        profile: Profile::Safe,
                        model: String::new(),
                        effort: Effort::Default,
                        attachments: Vec::new(),
                    };
                    write.send(Message::Text(json(&cmd).into())).await.unwrap();
                    println!("(sent message over websocket)");
                }
            }
            ServerEvent::History { items, .. } => println!("History: {} item(s)", items.len()),
            ServerEvent::Delta { text, .. } => assistant.push_str(&text),
            ServerEvent::Completed { is_error, .. } => {
                println!("assistant: {assistant}");
                println!("completed (is_error={is_error})");
                break;
            }
            ServerEvent::Error { detail, .. } => {
                println!("error: {detail}");
                break;
            }
            other => println!("{other:?}"),
        }
    }
}
