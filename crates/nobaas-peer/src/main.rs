//! `nobaas-peer`: an optional always-on peer. It keeps one identity across
//! restarts, hands out an invite, and relays between the peers linked to it
//! (the core's peer relay, rate-limited per peer). It sends no app data of its own.
//!
//! ```sh
//! nobaas-peer                 # run; prints the invite and writes it to $NOBAAS_DATA/invite
//! nobaas-peer check [FILE]    # handshake with the invite in FILE (default $NOBAAS_DATA/invite); exit 0 if it links
//! ```
//!
//! Settings, from the environment:
//!
//! | Variable | Default | |
//! |---|---|---|
//! | `NOBAAS_BIND` | `127.0.0.1:7700` | UDP address; `0.0.0.0:7700` to be reachable from other hosts |
//! | `NOBAAS_DATA` | unset | directory for `identity.key` and `invite`; unset: a new identity each run |
//! | `NOBAAS_STUN` | the default servers | `host:port` list, comma-separated; `none` turns STUN off. Never used on loopback |
//! | `NOBAAS_MAX_PEERS` | `256` | links over this many are closed |
//! | `NOBAAS_RELAY` | `1` | `0` stops relaying for others |
//! | `NOBAAS_RELAY_RATE` | `262144` | bytes per second each peer may have relayed |
//! | `NOBAAS_STATS_SECS` | `60` | how often to log a stats line; `0` never |

use nobaas::{Config, Event, Identity, Node, PeerId, DEFAULT_STUN_SERVERS};
use rand_core::{OsRng, RngCore};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        None | Some("run") => run(),
        Some("check") => check(args.get(1).map(PathBuf::from)),
        Some(other) => Err(format!("unknown command {other:?}; use `run` or `check [FILE]`").into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nobaas-peer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> Res<T> {
    match env(name) {
        Some(v) => v.parse().map_err(|_| format!("{name}={v:?} is not a number").into()),
        None => Ok(default),
    }
}

fn data_dir() -> Option<PathBuf> {
    env("NOBAAS_DATA").map(PathBuf::from)
}

/// The identity in `dir/identity.key` (32 secret bytes), created owner-only on first start.
fn load_identity(dir: &Path) -> Res<Identity> {
    let path = dir.join("identity.key");
    match fs::read(&path) {
        Ok(bytes) => {
            let secret: [u8; 32] = bytes.try_into().map_err(|_| format!("{} is not 32 bytes", path.display()))?;
            Ok(Identity::from_secret_bytes(&secret))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut secret = [0u8; 32];
            OsRng.fill_bytes(&mut secret);
            write_private(&path, &secret, true)?;
            eprintln!("new identity in {}", path.display());
            Ok(Identity::from_secret_bytes(&secret))
        }
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

/// Writes an owner-only (0600) file; with `new`, fails if it exists.
fn write_private(path: &Path, data: &[u8], new: bool) -> Res<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).mode(0o600);
    if new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    opts.open(path).and_then(|mut f| f.write_all(data)).map_err(|e| format!("{}: {e}", path.display()).into())
}

/// Replaces `dir/invite` through `invite.tmp` and a rename, so `check` never reads half an invite.
fn write_invite(dir: &Path, invite: &str) -> Res<()> {
    let (tmp, path) = (dir.join("invite.tmp"), dir.join("invite"));
    write_private(&tmp, invite.as_bytes(), false)?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()).into())
}

fn run() -> Res<()> {
    let bind = env("NOBAAS_BIND").unwrap_or_else(|| "127.0.0.1:7700".into());
    let max_peers: usize = env_num("NOBAAS_MAX_PEERS", 256)?;
    let stats_every = Duration::from_secs(env_num("NOBAAS_STATS_SECS", 60)?);
    let config = Config {
        relay: env("NOBAAS_RELAY").as_deref() != Some("0"),
        relay_rate: env_num("NOBAAS_RELAY_RATE", Config::default().relay_rate)?,
        ..Config::default()
    };
    let relay = config.relay;
    let stun: Vec<String> = match env("NOBAAS_STUN").as_deref() {
        Some("none") => Vec::new(),
        Some(list) => list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        None => DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
    };

    let dir = data_dir();
    let mut builder = Node::builder().config(config).stun_servers(stun);
    if let Some(dir) = &dir {
        builder = builder.identity(load_identity(dir)?);
    }
    let mut node = builder.bind(bind.as_str()).map_err(|e| format!("bind {bind}: {e}"))?;
    let invite = node.invite();
    if let Some(dir) = &dir {
        write_invite(dir, &invite)?;
    }
    println!("peer {} on {} (relay {relay}, max {max_peers} peers)", node.peer_id(), node.local_addr());
    println!("{invite}");

    let mut linked: HashSet<PeerId> = HashSet::new();
    // Refusals are counted for the stats line, not logged one by one: a handshake flood must not fill the disk.
    let mut refused: u64 = 0;
    let mut next_stats = Instant::now() + stats_every;
    let mut public = node.public_addr();
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, path, .. } => {
                    if linked.len() >= max_peers {
                        refused += 1;
                        node.disconnect(&peer);
                    } else {
                        linked.insert(peer);
                        println!("linked {peer} {path:?} ({} peers)", linked.len());
                    }
                }
                Event::Disconnected { peer, reason } if linked.remove(&peer) => {
                    println!("unlinked {peer} {reason:?} ({} peers)", linked.len());
                }
                // A relay only forwards: app messages addressed to it are dropped.
                _ => {}
            }
        }
        if node.public_addr() != public {
            public = node.public_addr();
            let invite = node.invite();
            if let Some(dir) = &dir {
                write_invite(dir, &invite)?;
            }
            println!("public address {public:?}; new invite:\n{invite}");
        }
        if !stats_every.is_zero() && Instant::now() >= next_stats {
            next_stats += stats_every;
            let s = node.stats();
            println!(
                "stats: {} peers, {} refused (over {max_peers}), rx {} tx {} packets, relayed {} dropped {}, rx auth failed {} malformed {}",
                linked.len(),
                refused,
                s.rx_packets,
                s.tx_packets,
                s.relayed,
                s.relay_dropped,
                s.rx_auth_failed,
                s.rx_malformed
            );
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(20)));
    }
}

/// Health check: links to the peer in the invite file from loopback, then closes.
fn check(file: Option<PathBuf>) -> Res<()> {
    let file =
        file.or_else(|| data_dir().map(|d| d.join("invite"))).ok_or("no invite file: pass one or set NOBAAS_DATA")?;
    let invite = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut node = Node::builder().stun_servers(Vec::<String>::new()).bind("127.0.0.1:0")?;
    let peer = node.connect(invite.trim())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer: p, .. } if p == peer => {
                    println!("ok: linked {peer}");
                    node.disconnect(&peer);
                    node.poll();
                    return Ok(());
                }
                Event::ConnectFailed { .. } => return Err(format!("could not reach {peer}").into()),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(format!("no link to {peer} within 5 s").into())
}
