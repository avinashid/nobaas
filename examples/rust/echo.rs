//! Echo over Nobaas: one process hosts, another joins with its invite.
//!
//! ```sh
//! cargo run -p nobaas --example echo                 # prints an nb: invite and echoes back
//! cargo run -p nobaas --example echo -- nb:…          # joins, sends "hello", prints the echo
//! ```

use nobaas::{Event, Lane, Node};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut node = Node::bind("0.0.0.0:0")?;
    match std::env::args().nth(1) {
        Some(invite) => {
            node.connect(&invite)?;
        }
        None => println!("{}", node.invite()),
    }
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
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
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}
