//! The team page's name on the network, published over mDNS by avahi for as long as the page
//! runs, so a link to it keeps working when another computer serves it.

use std::net::{IpAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How often the address the name points at is looked at again, as when DHCP changes it.
const LOOK: Duration = Duration::from_secs(30);

/// Publishes `name`, such as `team.local`, as this computer's address on its network.
pub fn publish(name: String) {
    std::thread::spawn(move || {
        let mut published: Option<(IpAddr, Child)> = None;
        loop {
            let address = address();
            let stale = match &mut published {
                Some((at, child)) => Some(*at) != address || !matches!(child.try_wait(), Ok(None)),
                None => true,
            };
            if stale {
                if let Some((_, mut child)) = published.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                if let Some(address) = address {
                    match spawn(&name, address) {
                        Ok(child) => {
                            println!("published {name} as {address}");
                            published = Some((address, child));
                        }
                        Err(error) => eprintln!("claudo: cannot publish {name}: {error}"),
                    }
                }
            }
            std::thread::sleep(LOOK);
        }
    });
}

fn spawn(name: &str, address: IpAddr) -> std::io::Result<Child> {
    // Without -R avahi also claims the address's reverse name, which the computer's own
    // name already holds.
    Command::new("avahi-publish")
        .args(["-a", "-R", name, &address.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

/// The address this computer reaches the network from. Connecting a UDP socket only picks
/// the route; nothing is sent.
fn address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}
