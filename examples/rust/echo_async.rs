//! Echo over Peerwire with tokio: one process hosts, another joins with its invite.
//!
//! ```sh
//! cargo run -p peerwire --features tokio --example echo_async           # prints an pw: invite
//! cargo run -p peerwire --features tokio --example echo_async -- pw:…   # joins and says hello
//! ```

use peerwire::{AsyncNode, Event, Lane};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut node = AsyncNode::bind("[::]:0").await?;
    match std::env::args().nth(1) {
        Some(invite) => {
            node.connect(&invite)?;
        }
        None => println!("{}", node.invite()),
    }
    loop {
        match node.next_event().await {
            Event::Connected { peer, .. } => {
                println!("connected to {peer}");
                node.send(&peer, Lane::Reliable, b"hello")?;
            }
            Event::Message { peer, data, .. } => {
                println!("{peer}: {}", String::from_utf8_lossy(&data));
                if data.starts_with(b"echo: ") {
                    return Ok(());
                }
                node.send(&peer, Lane::Reliable, &[b"echo: ", &data[..]].concat())?;
            }
            Event::ConnectFailed { peer } => return Err(format!("could not reach {peer}").into()),
            _ => {}
        }
    }
}
