//! Echo over Nobaas with tokio: one process hosts, another joins with its invite.
//!
//! ```sh
//! cargo run -p nobaas --features tokio --example echo_async           # prints an nb: invite
//! cargo run -p nobaas --features tokio --example echo_async -- nb:…   # joins and says hello
//! ```

use nobaas::{AsyncNode, Event, Lane};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut node = AsyncNode::bind("0.0.0.0:0").await?;
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
