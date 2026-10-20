//! Asks two public STUN servers, from one socket, for our public address.
//! The same answer from both means our NAT maps endpoint-independently (cone),
//! so hole punching can work; different ports mean a symmetric NAT.
//!
//! cargo run -p nobaas-core --example stun_probe

use nobaas_core::stun;
use rand_core::{OsRng, RngCore};
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    println!("local socket {}", sock.local_addr()?);
    let mut seen = Vec::new();
    for server in ["stun.l.google.com:19302", "stun.cloudflare.com:3478"] {
        let Some(addr) = server.to_socket_addrs()?.find(|a| a.is_ipv4()) else {
            println!("{server}: no IPv4 address");
            continue;
        };
        let mut tx = [0u8; 12];
        OsRng.fill_bytes(&mut tx);
        let mut answer = None;
        for _ in 0..3 {
            sock.send_to(&stun::binding_request(&tx), addr)?;
            let mut buf = [0u8; 1500];
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == addr => {
                    answer = stun::parse_binding_response(&buf[..n], &tx);
                    if answer.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }
        println!("{server} ({addr}): we are {answer:?}");
        seen.extend(answer);
    }
    match seen.as_slice() {
        [a, b] if a == b => println!("same mapping from both servers: endpoint-independent (cone) NAT or no NAT"),
        [a, b] => println!("different mappings ({a} vs {b}): symmetric NAT, expect relayed links"),
        _ => println!("not enough answers to classify"),
    }
    Ok(())
}
