//! A large arena over super-peers, on real peerwire-core endpoints in the simulator.
//!
//! Players are grouped into Hilbert islands of at most 12; each island is
//! served by one super-peer (a well-connected player elected to coalesce).
//! Every tick:
//!   player -> super-peer   INPUT: this tick's input plus the previous ones
//!                          (redundancy, so a lost packet costs nothing)
//!   super-peer -> player   BUNDLE: the island's new inputs, one packet per tick
//! plus island state to adjacent islands at 15 Hz, a minimap at 2 Hz and
//! cross-island projectile events stamped to land 4 ticks later. Players run
//! input-only lockstep on their island with the peerwire-game physics.
//!
//! Measured: confirmation lag (how far back a rollback must reach), late
//! projectile events, per-role bandwidth, and that every island member ends
//! with the same state CRC.
//!
//! cargo run --release -p peerwire-sim --example arena -- --players 512 --supers 8

#![allow(clippy::needless_range_loop)]

use peerwire_core::{Config, Event, Instant, Lane};
use peerwire_game::{
    compute_island_state_crc16, partition_into_hilbert_islands, q16_from_i32, PlayerEntityQ16, Vec2Q16,
};
use peerwire_sim::{NatConfig, NetConfig, Sim};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

const ARENA: u32 = 256;
const ISLAND: usize = 12;
const UNKNOWN: u16 = u16::MAX;
const STATE_BYTES: usize = 12 * 12;
/// Overload control sends bundles at least every this many ticks.
const MAX_INTERVAL: u32 = 6;
/// A player that gets no bundle for this many ticks moves its island to the
/// next super-peer in the island's ranking.
const MIGRATE_AFTER: u32 = 15;

pub struct Args {
    pub players: usize,
    pub supers: usize,
    pub secs: u32,
    pub hz: u32,
    pub latency_ms: u64,
    pub jitter_ms: u64,
    pub loss: f64,
    pub player_up_kbps: u64,
    pub super_up_mbps: u64,
    pub redundancy: usize,
    pub sym_pct: u64,
    pub proj_per_sec: f64,
    /// Ticks between a cross-island event and the tick it takes effect; 0
    /// derives it from measured delivery delays.
    pub flight: u32,
    /// Super-peer event handling per tick: inputs that arrive mid-tick are
    /// forwarded at the next sub-step (cut-through); resends go once per tick.
    pub substeps: u32,
    /// Super-peers repeat each member's latest input in every bundle.
    pub resend: bool,
    /// Interest radius (arena units) for neighbour-island state; 0 sends it to every member.
    pub aoi: u32,
    /// Island members connect to each other (through the super-peer, then hole
    /// punching) and send inputs directly where the path is direct.
    pub direct: bool,
    /// Projectile events use `Lane::Urgent` (sent twice) instead of `Lane::Reliable`.
    pub urgent: bool,
    /// A super-peer whose members report rising bundle delay (its uplink is
    /// queueing) sends bundles every 2nd or 3rd tick until the queue drains.
    pub adapt: bool,
    /// Uplink buffers (ms of queueing before tail drop); 0 is unlimited.
    pub buffer_ms: u64,
    /// Super-peer to kill (it goes silent) `kill_at` seconds into the game.
    pub kill: Option<usize>,
    pub kill_at: u32,
    pub seed: u64,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            players: 512,
            supers: 8,
            secs: 10,
            hz: 60,
            latency_ms: 20,
            jitter_ms: 10,
            loss: 0.01,
            player_up_kbps: 1000,
            super_up_mbps: 50,
            redundancy: 16,
            sym_pct: 20,
            proj_per_sec: 0.5,
            flight: 0,
            substeps: 1,
            resend: true,
            aoi: 24,
            direct: true,
            urgent: true,
            adapt: true,
            buffer_ms: 250,
            kill: None,
            kill_at: 4,
            seed: 1,
        }
    }
}

fn args() -> Args {
    let mut a = Args::default();
    let v: Vec<String> = std::env::args().skip(1).collect();
    for kv in v.chunks(2) {
        let val = kv.get(1).map(String::as_str).unwrap_or("");
        match kv[0].as_str() {
            "--players" => a.players = val.parse().unwrap(),
            "--supers" => a.supers = val.parse().unwrap(),
            "--secs" => a.secs = val.parse().unwrap(),
            "--hz" => a.hz = val.parse().unwrap(),
            "--latency" => a.latency_ms = val.parse().unwrap(),
            "--jitter" => a.jitter_ms = val.parse().unwrap(),
            "--loss" => a.loss = val.parse().unwrap(),
            "--player-up" => a.player_up_kbps = val.parse().unwrap(),
            "--super-up" => a.super_up_mbps = val.parse().unwrap(),
            "--redundancy" => a.redundancy = val.parse().unwrap(),
            "--sym" => a.sym_pct = val.parse().unwrap(),
            "--proj" => a.proj_per_sec = val.parse().unwrap(),
            "--flight" => a.flight = val.parse().unwrap(),
            "--substeps" => a.substeps = val.parse().unwrap(),
            "--resend" => a.resend = val.parse().unwrap(),
            "--aoi" => a.aoi = val.parse().unwrap(),
            "--direct" => a.direct = val.parse().unwrap(),
            "--urgent" => a.urgent = val.parse().unwrap(),
            "--adapt" => a.adapt = val.parse().unwrap(),
            "--buffer-ms" => a.buffer_ms = val.parse().unwrap(),
            "--kill" => a.kill = Some(val.parse().unwrap()),
            "--kill-at" => a.kill_at = val.parse().unwrap(),
            "--seed" => a.seed = val.parse().unwrap(),
            k => panic!("unknown flag {k}"),
        }
    }
    a
}

/// Deterministic input policy: change direction now and then, dash sometimes.
fn input_bits(player: usize, tick: u32) -> u16 {
    let phase = (tick + player as u32 * 7) / 30;
    let h = (player as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (phase as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    let h = h ^ (h >> 29);
    let dir = [0x01, 0x02, 0x04, 0x08, 0x05, 0x09, 0x06, 0x0a][(h % 8) as usize];
    dir | if (h >> 8).is_multiple_of(5) { 0x10 } else { 0 }
}

fn hash01(a: u64, b: u64) -> f64 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 32;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 32;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Run-length codes the `n` inputs ending at tick `t`, newest first: a run
/// count, then (length, bits) per run. Inputs change every few dozen ticks, so
/// a redundancy window costs ~4 bytes whatever its length.
fn put_runs(out: &mut Vec<u8>, known: &[u16], t: usize, n: usize) {
    let at = out.len();
    out.push(0);
    let mut k = 0;
    while k < n {
        let bits = known[t - k];
        let mut len = 1;
        while k + len < n && len < 255 && known[t - k - len] == bits {
            len += 1;
        }
        out.push(len as u8);
        out.extend_from_slice(&bits.to_le_bytes());
        out[at] += 1;
        k += len;
    }
}

/// Writes the runs that `put_runs` encoded into `known`, ending at tick `t`;
/// returns the bytes consumed.
fn take_runs(b: &[u8], t: usize, known: &mut [u16]) -> usize {
    let mut i = 1;
    let mut tick = t;
    for _ in 0..b[0] {
        let bits = u16::from_le_bytes([b[i + 1], b[i + 2]]);
        for _ in 0..b[i] {
            // A repair may cover ticks the super-peer never got.
            if bits != UNKNOWN {
                known[tick] = bits;
            }
            tick = tick.wrapping_sub(1);
        }
        i += 3;
    }
    i
}

/// Advances how far `slot`'s inputs are known without a gap.
fn extend_contiguous(p: &mut Player, slot: usize, ticks: u32) {
    while (p.contiguous[slot] as usize) < ticks as usize && p.known[slot][p.contiguous[slot] as usize] != UNKNOWN {
        p.contiguous[slot] += 1;
    }
}

fn cell(q: i32) -> u8 {
    (q >> 16).clamp(0, ARENA as i32 - 1) as u8
}

struct Player {
    node: usize,
    island: usize,
    /// Position in the island's member list.
    slot: usize,
    /// known[slot][tick] = input bits, or UNKNOWN.
    known: Vec<Vec<u16>>,
    contiguous: Vec<u32>,
    confirmed: u32,
    /// Own entity, stepped locally each tick to report a position for interest management.
    ent: PlayerEntityQ16,
    /// How late bundles arrive (ms past their tick), rising fast and decaying slowly.
    delay_ms: f64,
    /// The same, averaged.
    delay_avg_ms: f64,
    /// Per island: delivery time from our super-peer to its slowest member (2 ms units).
    deliver: Vec<u8>,
    /// Per slot: tick of the last NACK sent for it.
    nacked: Vec<u32>,
    /// The super-peer currently serving our island.
    sup: usize,
    /// Tick a bundle last arrived (or the link to `sup` came up).
    last_bundle: u32,
    /// The link to `sup` is up.
    sup_up: bool,
    /// After a migration, send a long input history so the new super-peer
    /// can fill the outage.
    rejoin_until: u32,
    /// Events sent in the last two seconds, resent after a migration.
    recent_events: VecDeque<(u32, Vec<u8>)>,
    /// Events received, by (emitter, tick): resends arrive twice.
    seen_events: HashSet<(u16, u32)>,
}

/// A member's latest INPUT, as its super-peer keeps it.
#[derive(Default)]
struct Latest {
    tick: u32,
    runs: Vec<u8>,
    /// Not yet in a bundle.
    new: bool,
    pos: (u8, u8),
    /// Island slots the member reaches directly.
    direct: u16,
    /// How late the member sees this super-peer's bundles (units of 2 ms):
    /// near the recent maximum, for landing budgets...
    delay: u8,
    /// ...and on average, for overload detection.
    delay_avg: u8,
}

struct Super {
    node: usize,
    islands: Vec<usize>,
    latest: HashMap<usize, Latest>,
    /// Bundles go out every `interval` ticks (1 normally, more when overloaded).
    interval: u32,
    last_change: u32,
    /// Queueing measured when the interval last went up.
    q_at_raise: f64,
    /// Every member's inputs as received, to answer NACKs.
    hist: HashMap<usize, Vec<u16>>,
    /// Other super-peers' islands: their slowest member's bundle delay (2 ms units).
    remote_delay: HashMap<usize, u8>,
    /// State and minimap held for the next bundle tick, to share its packet.
    held: Vec<(usize, Lane, Vec<u8>)>,
    /// Which super-peer serves each island, as far as this one knows.
    owner_view: Vec<usize>,
    /// Events forwarded to other super-peers in the last two seconds (tick,
    /// target island, message), re-forwarded if the island changes owner.
    fwd_log: VecDeque<(u32, usize, Vec<u8>)>,
    dead: bool,
}

fn pctl(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// What one run measured.
#[derive(Debug)]
pub struct Summary {
    pub islands: usize,
    pub agree: usize,
    pub stalled: usize,
    pub lag_p50_ms: f64,
    pub lag_p99_ms: f64,
    pub player_down_kbps: f64,
    pub super_up_mbps: f64,
}

#[allow(dead_code)]
fn main() {
    run(args());
}

pub fn run(a: Args) -> Summary {
    let wall = std::time::Instant::now();
    let ticks = a.secs * a.hz;
    let tick_at = |t: u32| Instant::from_micros(t as u64 * 1_000_000 / a.hz as u64);
    let net = NetConfig {
        latency: Duration::from_millis(a.latency_ms),
        jitter: Duration::from_millis(a.jitter_ms),
        loss: a.loss,
        duplicate: 0.0,
        in_order: false,
    };
    let mut sim = Sim::new(net, a.seed);
    // A long handshake timeout so heavy-loss runs still bring every link up.
    let config = Config { unreliable_queue: 1024, handshake_timeout: Duration::from_secs(30), ..Config::default() };
    let mut rng = ChaCha20Rng::seed_from_u64(a.seed ^ 0xa5a5);

    // World and islands.
    let entities: Vec<PlayerEntityQ16> = (0..a.players)
        .map(|i| PlayerEntityQ16 {
            peer_id: i as u16,
            pos: Vec2Q16::new(
                q16_from_i32((rng.next_u32() % ARENA) as i32),
                q16_from_i32((rng.next_u32() % ARENA) as i32),
            ),
            vel: Vec2Q16::new(0, 0),
            hp: 100,
            last_input_bits: 0,
        })
        .collect();
    let islands: Vec<Vec<usize>> = partition_into_hilbert_islands(ARENA, &entities, ISLAND)
        .into_iter()
        .map(|c| c.players.iter().map(|p| p.peer_id as usize).collect())
        .collect();
    let n_islands = islands.len();
    let owner_of = |k: usize| k * a.supers / n_islands;
    // Each island's ranked super-peers: its owner, then the rest in an order
    // hashed per island, so a dead super-peer's islands spread over the others.
    let ranked = |k: usize| {
        let mut r: Vec<usize> = (0..a.supers).filter(|&s| s != owner_of(k)).collect();
        r.sort_by(|&x, &y| hash01(k as u64, x as u64).partial_cmp(&hash01(k as u64, y as u64)).unwrap());
        r.insert(0, owner_of(k));
        r
    };

    // Nodes: super-peers are public with fast uplinks; players sit behind NATs.
    let mut supers: Vec<Super> = (0..a.supers)
        .map(|s| {
            let node = sim.add_node(config.clone());
            sim.nodes[node].uplink_bps = Some(a.super_up_mbps * 1_000_000);
            sim.nodes[node].uplink_buffer = (a.buffer_ms > 0).then(|| Duration::from_millis(a.buffer_ms));
            Super {
                node,
                islands: (0..n_islands).filter(|&k| owner_of(k) == s).collect(),
                latest: HashMap::new(),
                interval: 1,
                last_change: 0,
                q_at_raise: 0.0,
                hist: HashMap::new(),
                remote_delay: HashMap::new(),
                held: Vec::new(),
                owner_view: (0..n_islands).map(owner_of).collect(),
                fwd_log: VecDeque::new(),
                dead: false,
            }
        })
        .collect();
    let mut players: Vec<Player> = Vec::with_capacity(a.players);
    let mut island_of = vec![(0usize, 0usize); a.players];
    for (k, members) in islands.iter().enumerate() {
        for (slot, &p) in members.iter().enumerate() {
            island_of[p] = (k, slot);
        }
    }
    for p in 0..a.players {
        let nat = if (p as u64 * 100 / a.players as u64) < a.sym_pct {
            NatConfig::symmetric_random()
        } else {
            NatConfig::port_restricted()
        };
        let n = sim.add_nat(nat, None);
        let node = sim.add_node_behind(n, config.clone());
        sim.nodes[node].uplink_bps = Some(a.player_up_kbps * 1000);
        sim.nodes[node].uplink_buffer = (a.buffer_ms > 0).then(|| Duration::from_millis(a.buffer_ms));
        let (island, slot) = island_of[p];
        let size = islands[island].len();
        players.push(Player {
            node,
            island,
            slot,
            known: vec![vec![UNKNOWN; ticks as usize]; size],
            contiguous: vec![0; size],
            confirmed: 0,
            ent: entities[p],
            delay_ms: 0.0,
            delay_avg_ms: 0.0,
            deliver: Vec::new(),
            nacked: vec![0; size],
            sup: owner_of(island),
            last_bundle: 0,
            sup_up: true,
            rejoin_until: 0,
            recent_events: VecDeque::new(),
            seen_events: HashSet::new(),
        });
    }
    let node_to_super: HashMap<usize, usize> = supers.iter().enumerate().map(|(i, s)| (s.node, i)).collect();
    let player_of_node: HashMap<usize, usize> = players.iter().enumerate().map(|(i, p)| (p.node, i)).collect();
    let peer_to_node: HashMap<_, _> = (0..sim.nodes.len()).map(|i| (sim.peer_id(i), i)).collect();

    // Links: each player to its island's super-peer, super-peers in a full mesh.
    for p in 0..a.players {
        let s = owner_of(players[p].island);
        sim.connect(players[p].node, supers[s].node).unwrap();
    }
    for i in 0..a.supers {
        for j in i + 1..a.supers {
            sim.connect(supers[i].node, supers[j].node).unwrap();
        }
    }
    let want = a.players + a.supers * (a.supers - 1) / 2;
    // A dial gives up after its handshake attempts; like an app, redial.
    let mut up = false;
    for _ in 0..4 {
        up = sim.run_until(Duration::from_secs(15), |s| {
            let links: usize = supers.iter().map(|sp| s.nodes[sp.node].endpoint.connected_peers().count()).sum();
            links >= a.players + a.supers * (a.supers - 1)
        });
        if up {
            break;
        }
        for p in 0..a.players {
            let (node, sp) = (players[p].node, supers[owner_of(players[p].island)].node);
            if sim.nodes[node].endpoint.session_count() == 0 {
                sim.connect(node, sp).unwrap();
            }
        }
    }
    if !up {
        let links: usize = supers.iter().map(|sp| sim.nodes[sp.node].endpoint.connected_peers().count()).sum();
        let (mut failed, mut gone) = (0, Vec::new());
        for i in 0..sim.nodes.len() {
            for e in sim.take_events(i) {
                match e {
                    Event::ConnectFailed { .. } => failed += 1,
                    Event::Disconnected { reason, .. } => gone.push(format!("{reason:?}")),
                    _ => {}
                }
            }
        }
        println!("disconnects during setup: {gone:?}");
        for p in 0..a.players {
            let sp = supers[owner_of(players[p].island)].node;
            let (pid, sid) = (sim.peer_id(players[p].node), sim.peer_id(sp));
            let (pe, se) = (&sim.nodes[players[p].node].endpoint, &sim.nodes[sp].endpoint);
            if !se.is_connected(&pid) {
                println!(
                    "player {p}: player sees super connected={} path={:?} sessions={}; super sees player path={:?} hs_dropped={}",
                    pe.is_connected(&sid),
                    pe.path(&sid),
                    pe.session_count(),
                    se.path(&pid),
                    se.stats().rx_handshake_dropped
                );
            }
        }
        panic!(
            "not all {want} links came up: {links} super-peer link ends of {}, {failed} ConnectFailed",
            a.players + a.supers * (a.supers - 1)
        );
    }
    // Hot standby: every player also keeps an idle link to the next super-peer
    // in its island's ranking, so failing over needs no handshake.
    if a.supers > 1 {
        for p in 0..a.players {
            let _ = sim.connect(players[p].node, supers[ranked(players[p].island)[1]].node);
        }
    }
    // Island mesh: members dial each other through their super-peer; the SDK
    // punches and moves each link to a direct path where the NATs allow.
    let node_of: Vec<usize> = players.iter().map(|p| p.node).collect();
    let peer_of: Vec<peerwire_core::PeerId> = node_of.iter().map(|&n| sim.peer_id(n)).collect();
    let mut direct_pairs = (0usize, 0usize);
    if a.direct {
        for (k, members) in islands.iter().enumerate() {
            let sp = supers[owner_of(k)].node;
            for i in 0..members.len() {
                for j in i + 1..members.len() {
                    sim.connect_via(node_of[members[i]], sp, node_of[members[j]], &[]).unwrap();
                }
            }
        }
        let pairs: usize = islands.iter().map(|m| m.len() * (m.len() - 1) / 2).sum();
        sim.run_until(Duration::from_secs(15), |s| {
            islands.iter().all(|m| {
                m.iter().all(|&x| m.iter().all(|&y| x == y || s.nodes[node_of[x]].endpoint.is_connected(&peer_of[y])))
            })
        });
        // Let hole punching run its course.
        sim.run_for(Duration::from_secs(6));
        let direct = islands
            .iter()
            .flat_map(|m| m.iter().flat_map(move |&x| m.iter().map(move |&y| (x, y))))
            .filter(|&(x, y)| {
                x < y
                    && matches!(sim.nodes[node_of[x]].endpoint.path(&peer_of[y]), Some(peerwire_core::Path::Direct(_)))
            })
            .count();
        direct_pairs = (direct, pairs);
    }
    let t_setup = sim.now;
    for i in 0..sim.nodes.len() {
        sim.take_events(i);
        let n = &mut sim.nodes[i];
        (n.tx_bytes, n.rx_bytes, n.tx_packets, n.rx_packets, n.uplink_dropped) = (0, 0, 0, 0, 0);
    }
    println!(
        "{} players in {} islands, {} super-peers ({:.0} players each), links up in {:?} virtual",
        a.players,
        n_islands,
        a.supers,
        a.players as f64 / a.supers as f64,
        Duration::from_micros(t_setup.as_micros())
    );

    // Run. Game time starts at t0.
    let t0 = sim.now;
    let at = |t: u32| t0 + Duration::from_micros(tick_at(t).as_micros());
    let mut lags = Vec::new();
    let mut proj_lat = Vec::new();
    let (mut proj_sent, mut proj_late, mut proj_recv) = (0u64, 0u64, 0u64);
    let mut state_recv = 0u64;
    let warmup = a.hz; // ignore the first second
    let mut send_err = 0u64;
    let (mut sp_kind_bytes, mut sp_kind_msgs) = ([0u64; 10], [0u64; 10]);
    let super_peer: Vec<peerwire_core::PeerId> = supers.iter().map(|s| sim.peer_id(s.node)).collect();
    let tick_ms = 1000.0 / a.hz as f64;
    let (mut flight_sum, mut proj_emits) = (0u64, 0u64);
    let (mut nacks, mut repairs) = (0u64, 0u64);
    let kill_tick = a.kill.map(|_| a.kill_at * a.hz);
    let orphaned: Vec<usize> = a.kill.map_or(Vec::new(), |s| (0..n_islands).filter(|&k| owner_of(k) == s).collect());
    let mut migrations = 0u64;
    let mut event_resends = 0u64;
    let mut fail_lags: Vec<f64> = Vec::new();
    let mut moved_at: HashMap<usize, u32> = HashMap::new();
    let mut interval_ticks = [0u64; MAX_INTERVAL as usize + 1];
    let mut interval_changes = 0u64;
    for t in 0..ticks + a.hz {
        let live = t < ticks;
        // Island slots each player currently reaches over a direct path.
        let masks: Vec<u16> = (0..players.len())
            .map(|pi| {
                if !a.direct {
                    return 0;
                }
                let ep = &sim.nodes[node_of[pi]].endpoint;
                islands[players[pi].island].iter().enumerate().fold(0u16, |m, (slot, &q)| {
                    if q != pi && matches!(ep.path(&peer_of[q]), Some(peerwire_core::Path::Direct(_))) {
                        m | 1 << slot
                    } else {
                        m
                    }
                })
            })
            .collect();
        if Some(t) == kill_tick {
            let s = a.kill.unwrap();
            supers[s].dead = true;
            let dead = sim.addr(supers[s].node);
            sim.hook = Some(Box::new(move |from, to, _| from != dead && to != dead));
        }
        // Players emit.
        {
            for (pi, p) in players.iter_mut().enumerate() {
                // Super-peer silent: move to the next in the island's ranking.
                // Every member computes the same choice, so no vote is needed.
                // The silence clock starts once the link is up (a handshake retry
                // takes 500 ms); a dial that never comes up gets two seconds.
                let up = sim.nodes[p.node].endpoint.is_connected(&super_peer[p.sup]);
                if up && !p.sup_up {
                    p.last_bundle = t;
                }
                p.sup_up = up;
                let patience = if up { MIGRATE_AFTER } else { 2 * a.hz };
                if t > a.hz && t > p.last_bundle + patience {
                    let r = ranked(p.island);
                    let pos = r.iter().position(|&x| x == p.sup).unwrap();
                    p.sup = r[(pos + 1) % r.len()];
                    p.last_bundle = t;
                    p.sup_up = false;
                    p.rejoin_until = t + 60;
                    let _ = sim.connect(p.node, supers[p.sup].node);
                    // The next standby.
                    let _ = sim.connect(p.node, supers[r[(pos + 2) % r.len()]].node);
                    // Events that may have died with the old super-peer go again.
                    let lane = if a.urgent { Lane::Urgent } else { Lane::Reliable };
                    for (_, e) in &p.recent_events {
                        let _ = sim.send(p.node, supers[p.sup].node, lane, e);
                        event_resends += 1;
                    }
                    migrations += 1;
                    moved_at.entry(p.island).or_insert(t);
                }
                // After the last tick, keep repeating the final input (the game
                // would keep ticking) so redundancy can fill any late gap.
                let t = t.min(ticks - 1);
                if live {
                    let bits = input_bits(pi, t);
                    p.known[p.slot][t as usize] = bits;
                    let own = p.slot;
                    extend_contiguous(p, own, ticks);
                    p.ent.step_physics(bits, q16_from_i32(ARENA as i32 - 1));
                }
                let n = (t as usize + 1).min(if t < p.rejoin_until { 64 } else { a.redundancy });
                let mut m = vec![1u8];
                m.extend_from_slice(&(pi as u16).to_le_bytes());
                m.extend_from_slice(&t.to_le_bytes());
                m.extend_from_slice(&[cell(p.ent.pos.x), cell(p.ent.pos.y)]);
                m.extend_from_slice(&masks[pi].to_le_bytes());
                m.push((p.delay_ms / 2.0).round().min(255.0) as u8);
                m.push((p.delay_avg_ms / 2.0).round().min(255.0) as u8);
                put_runs(&mut m, &p.known[p.slot], t as usize, n);
                let sp = supers[p.sup].node;
                if up && sim.send(p.node, sp, Lane::Unreliable, &m).is_err() {
                    send_err += 1;
                }
                for (slot, &q) in islands[p.island].iter().enumerate() {
                    if masks[pi] >> slot & 1 == 1 && sim.send(p.node, node_of[q], Lane::Unreliable, &m).is_err() {
                        send_err += 1;
                    }
                }
                // NACK a member whose inputs are stuck past the redundancy window.
                let stuck_before = t.saturating_sub(a.redundancy as u32 + 4);
                for slot in 0..p.contiguous.len() {
                    if slot != p.slot && p.contiguous[slot] < stuck_before && t >= p.nacked[slot] + 6 {
                        p.nacked[slot] = t;
                        let mut nk = vec![8u8, slot as u8];
                        nk.extend_from_slice(&p.contiguous[slot].to_le_bytes());
                        let _ = sim.send(p.node, sp, Lane::Unreliable, &nk);
                        nacks += 1;
                    }
                }
                if !live {
                    continue;
                }
                if p.slot == 0 && t % 4 == 0 {
                    let mut st = vec![3u8];
                    st.extend_from_slice(&(p.island as u16).to_le_bytes());
                    st.extend_from_slice(&t.to_le_bytes());
                    // Island bounding box, filled in by the super-peer.
                    st.resize(11 + STATE_BYTES, 0);
                    let _ = sim.send(p.node, sp, Lane::Unreliable, &st);
                }
                if hash01(pi as u64, t as u64) < a.proj_per_sec / a.hz as f64 {
                    let target = if p.island + 1 < n_islands { p.island + 1 } else { p.island - 1 };
                    let mut pr = vec![4u8];
                    pr.extend_from_slice(&(target as u16).to_le_bytes());
                    pr.extend_from_slice(&t.to_le_bytes());
                    // Landing tick: far enough out to cover this hop plus our
                    // super-peer's measured delivery time to the target island.
                    // Until the minimap reports the target's delivery time, assume
                    // the other two hops are like our own.
                    let up =
                        sim.nodes[p.node].endpoint.rtt(&super_peer[p.sup]).map_or(25.0, |r| r.as_secs_f64() * 500.0);
                    let rest = match p.deliver.get(target) {
                        Some(&d) if d > 0 => 2.0 * d as f64,
                        _ => 2.0 * up + tick_ms,
                    };
                    // +2 ticks: our super-peer handles events once per tick, and rounding.
                    let flight = if a.flight > 0 { a.flight } else { ((up + rest) * 1.3 / tick_ms).ceil() as u32 + 2 };
                    flight_sum += flight as u64;
                    proj_emits += 1;
                    pr.extend_from_slice(&(t + flight).to_le_bytes());
                    pr.extend_from_slice(&(pi as u16).to_le_bytes());
                    pr.resize(32, 0);
                    let lane = if a.urgent { Lane::Urgent } else { Lane::Reliable };
                    let _ = sim.send(p.node, sp, lane, &pr);
                    p.recent_events.push_back((t, pr));
                    while p.recent_events.front().is_some_and(|e| e.0 + 2 * a.hz < t) {
                        p.recent_events.pop_front();
                    }
                    proj_sent += islands[target].len() as u64;
                }
            }
        }
        // Super-peers handle what arrived: events are forwarded at once (every
        // sub-step), input bundles go out once per tick.
        for sub in 0..a.substeps {
            // Super-peers coalesce what arrived during the last tick.
            for si in 0..supers.len() {
                if supers[si].dead {
                    continue;
                }
                let node = supers[si].node;
                while supers[si].fwd_log.front().is_some_and(|e| e.0 + 2 * a.hz < t) {
                    supers[si].fwd_log.pop_front();
                }
                let mut out: Vec<(usize, Lane, Vec<u8>)> = Vec::new();
                for e in sim.take_events(node) {
                    let Event::Message { peer, data, .. } = e else { continue };
                    let from = peer_to_node[&peer];
                    match data.first() {
                        Some(1) => {
                            let pl = u16::from_le_bytes([data[1], data[2]]) as usize;
                            let tk = u32::from_le_bytes(data[3..7].try_into().unwrap());
                            // An island whose super-peer failed moves here: adopt it.
                            let k = players[pl].island;
                            if supers[si].owner_view[k] != si {
                                supers[si].owner_view[k] = si;
                                supers[si].islands.push(k);
                            }
                            let h = supers[si].hist.entry(pl).or_insert_with(|| vec![UNKNOWN; ticks as usize]);
                            take_runs(&data[13..], tk as usize, h);
                            let ent = supers[si].latest.entry(pl).or_default();
                            if ent.runs.is_empty() || tk >= ent.tick {
                                *ent = Latest {
                                    tick: tk,
                                    runs: data[13..].to_vec(),
                                    new: true,
                                    pos: (data[7], data[8]),
                                    direct: u16::from_le_bytes([data[9], data[10]]),
                                    delay: data[11],
                                    delay_avg: data[12],
                                };
                            }
                        }
                        Some(8) => {
                            // NACK: a member is stuck on a gap; resend that range of
                            // inputs from the history.
                            let me = player_of_node[&from];
                            let (slot, start) = (data[1] as usize, u32::from_le_bytes(data[2..6].try_into().unwrap()));
                            if let Some(h) = islands[players[me].island].get(slot).and_then(|m| supers[si].hist.get(m))
                            {
                                let end = (start + 63).min(ticks - 1);
                                let mut m = vec![9u8, slot as u8];
                                m.extend_from_slice(&end.to_le_bytes());
                                put_runs(&mut m, h, end as usize, (end - start + 1) as usize);
                                out.push((from, Lane::Unreliable, m));
                                repairs += 1;
                            }
                        }
                        Some(6) => {
                            // Another super-peer's islands and their slowest member's delay.
                            let o = node_to_super[&from];
                            for e in data[1..].as_chunks::<3>().0.iter() {
                                let k = u16::from_le_bytes([e[0], e[1]]) as usize;
                                supers[si].remote_delay.insert(k, e[2]);
                                if supers[si].owner_view[k] != si {
                                    if supers[si].owner_view[k] != o {
                                        // The island moved: events we sent its old owner
                                        // may be lost, so they go to the new one.
                                        supers[si].owner_view[k] = o;
                                        let lane = if a.urgent { Lane::Urgent } else { Lane::Reliable };
                                        for (_, tk, m) in supers[si].fwd_log.iter().filter(|e| e.1 == k) {
                                            let _ = tk;
                                            out.push((supers[o].node, lane, m.clone()));
                                            event_resends += 1;
                                        }
                                    }
                                } else {
                                    // Both claim the island: the one ranked lower for it yields.
                                    let r = ranked(k);
                                    if r.iter().position(|&x| x == o) < r.iter().position(|&x| x == si) {
                                        supers[si].owner_view[k] = o;
                                        supers[si].islands.retain(|&x| x != k);
                                    }
                                }
                            }
                        }
                        // STATE from an island leader goes to the adjacent islands; a
                        // projectile to its target island; FWD_STATE is STATE that
                        // another super-peer handed us for one of our islands.
                        Some(&kind @ (3 | 4 | 7)) => {
                            let k = u16::from_le_bytes([data[1], data[2]]) as usize;
                            // Overloaded: forward only every interval-th neighbour state.
                            let st_tick = u32::from_le_bytes(data[3..7].try_into().unwrap());
                            if kind != 4 && (st_tick / 4) % supers[si].interval != 0 {
                                continue;
                            }
                            let targets: Vec<usize> = match kind {
                                3 => [k.checked_sub(1), Some(k + 1).filter(|&x| x < n_islands)]
                                    .into_iter()
                                    .flatten()
                                    .collect(),
                                _ => vec![k],
                            };
                            let lane = match (kind, a.urgent) {
                                (4, true) => Lane::Urgent,
                                (4, false) => Lane::Reliable,
                                _ => Lane::Unreliable,
                            };
                            let mut data = data;
                            if kind == 3 && !node_to_super.contains_key(&from) {
                                // Stamp the source island's bounding box from its members' positions.
                                let mut bb = [u8::MAX, u8::MAX, 0, 0];
                                for &m in &islands[k] {
                                    if let Some(&Latest { pos: (x, y), .. }) = supers[si].latest.get(&m) {
                                        bb = [bb[0].min(x), bb[1].min(y), bb[2].max(x), bb[3].max(y)];
                                    }
                                }
                                data[7..11].copy_from_slice(&bb);
                            }
                            for tk in targets {
                                let o = supers[si].owner_view[tk];
                                if o == si {
                                    let mut m = data.clone();
                                    if kind == 7 {
                                        m[0] = 3;
                                    }
                                    // Interest management: neighbour state only reaches members
                                    // within `aoi` of the source island.
                                    let r = a.aoi as i32;
                                    let near = |x: usize| {
                                        if m[0] != 3 || r == 0 {
                                            return true;
                                        }
                                        let Some(&Latest { pos: (px, py), .. }) = supers[si].latest.get(&x) else {
                                            return true;
                                        };
                                        let (px, py) = (px as i32, py as i32);
                                        px >= m[7] as i32 - r
                                            && px <= m[9] as i32 + r
                                            && py >= m[8] as i32 - r
                                            && py <= m[10] as i32 + r
                                    };
                                    for &x in &islands[tk] {
                                        if near(x) {
                                            out.push((players[x].node, lane, m.clone()));
                                        }
                                    }
                                } else if !node_to_super.contains_key(&from) {
                                    let mut m = data.clone();
                                    if kind == 3 {
                                        m[0] = 7;
                                        m[1..3].copy_from_slice(&(tk as u16).to_le_bytes());
                                    }
                                    if kind == 4 {
                                        supers[si].fwd_log.push_back((t, tk, m.clone()));
                                    }
                                    out.push((supers[o].node, lane, m));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                {
                    // Bundles: the full one (with resends) once per tick; on later
                    // sub-steps, cut-through of inputs that arrived since.
                    let full = sub == 0;
                    if full && a.adapt {
                        // Overload control from the members' delay reports.
                        // Queueing = how late members see bundles, less the path's
                        // one-way delay (half the link's minimum RTT, handshake included).
                        let ep = &sim.nodes[node].endpoint;
                        let sp = &mut supers[si];
                        let ds: Vec<f64> = sp
                            .latest
                            .iter()
                            .filter_map(|(m, l)| {
                                let base = ep.min_rtt(&peer_of[*m])?.as_secs_f64() * 500.0;
                                Some(l.delay_avg as f64 * 2.0 - base)
                            })
                            .collect();
                        if !ds.is_empty() && t > a.hz {
                            // A standing queue delays every member; where a member's
                            // bundle falls in a tick's burst only delays some. So use
                            // a low percentile.
                            let mut ds = ds;
                            let q = pctl(&mut ds, 0.1);
                            // Step up again only if the last step isn't already draining the queue.
                            let draining = sp.q_at_raise > 0.0 && q < 0.7 * sp.q_at_raise;
                            if t >= sp.last_change + a.hz / 2 && q > 25.0 && !draining && sp.interval < MAX_INTERVAL {
                                sp.interval += 1;
                                sp.last_change = t;
                                sp.q_at_raise = q;
                                interval_changes += 1;
                            } else if t >= sp.last_change + 2 * a.hz && q < 10.0 && sp.interval > 1 {
                                sp.q_at_raise = 0.0;
                                sp.interval -= 1;
                                sp.last_change = t;
                                interval_changes += 1;
                            }
                        }
                    }
                    if full {
                        interval_ticks[supers[si].interval as usize] += 1;
                    }
                    // Islands take turns (staggered by index) so an interval > 1
                    // spreads the load instead of bursting it every few ticks.
                    let interval = supers[si].interval;
                    let due = |k: usize| (t as usize + k).is_multiple_of(interval as usize);
                    for &k in &supers[si].islands.clone() {
                        if full && !due(k) {
                            continue;
                        }
                        let mut entries: Vec<Option<Vec<u8>>> = vec![None; islands[k].len()];
                        for (slot, &m) in islands[k].iter().enumerate() {
                            // Every member's latest input goes in every bundle while it
                            // is recent, not only when new: under jitter a member's
                            // newest tick advances in jumps, and sending each entry
                            // once would leave a tick covered by only 2-3 packets.
                            if let Some(ent) = supers[si].latest.get_mut(&m) {
                                if (ent.new || (full && a.resend && ent.tick + a.redundancy as u32 > t))
                                    && t - ent.tick < 256
                                {
                                    let mut e = vec![slot as u8, (t - ent.tick) as u8];
                                    e.extend_from_slice(&ent.runs);
                                    entries[slot] = Some(e);
                                    ent.new = false;
                                }
                            }
                        }
                        // Each member gets the inputs it doesn't have: not its own,
                        // nor those of members it hears from directly.
                        for (rs, &r) in islands[k].iter().enumerate() {
                            let skip = supers[si].latest.get(&r).map_or(0, |e| e.direct) | 1 << rs;
                            let mut bundle = vec![2u8];
                            bundle.extend_from_slice(&t.to_le_bytes());
                            bundle.push(0);
                            for (slot, e) in entries.iter().enumerate() {
                                if let (Some(e), false) = (e, skip >> slot & 1 == 1) {
                                    bundle.extend_from_slice(e);
                                    bundle[5] += 1;
                                }
                            }
                            // An empty bundle still goes out every few ticks: it is the
                            // members' proof that their super-peer is alive.
                            if bundle[5] > 0 || interval > 1 || (t as usize + k).is_multiple_of(5) {
                                out.push((players[r].node, Lane::Unreliable, bundle));
                            }
                        }
                    }
                    if full && t % ((a.hz / 2).max(1) * supers[si].interval) == 0 {
                        // Slowest member's bundle delay per own island.
                        let sp = &supers[si];
                        let own: Vec<(usize, u8)> = sp
                            .islands
                            .iter()
                            .map(|&k| {
                                (
                                    k,
                                    islands[k]
                                        .iter()
                                        .filter_map(|m| sp.latest.get(m))
                                        .map(|l| l.delay)
                                        .max()
                                        .unwrap_or(0),
                                )
                            })
                            .collect();
                        // Minimap: per island a quantised centroid (2 bytes) and the
                        // delivery time from this super-peer to its slowest member.
                        let mut mm = vec![5u8];
                        mm.extend_from_slice(&(n_islands as u16).to_le_bytes());
                        for j in 0..n_islands {
                            let o = sp.owner_view[j];
                            let d = if o == si {
                                own.iter().find(|e| e.0 == j).map_or(0, |e| e.1) as f64 * 2.0
                            } else {
                                let hop = sim.nodes[node]
                                    .endpoint
                                    .rtt(&super_peer[o])
                                    .map_or(0.0, |r| r.as_secs_f64() * 500.0);
                                // Plus up to a tick waiting at the other super-peer.
                                hop + sp.remote_delay.get(&j).map_or(0, |&d| d) as f64 * 2.0 + 1000.0 / a.hz as f64
                            };
                            mm.extend_from_slice(&[0, 0, (d / 2.0).round().min(255.0) as u8]);
                        }
                        for &k in &sp.islands {
                            for &m in &islands[k] {
                                out.push((players[m].node, Lane::Unreliable, mm.clone()));
                            }
                        }
                        let mut occ = vec![6u8];
                        for (k, d) in own {
                            occ.extend_from_slice(&(k as u16).to_le_bytes());
                            occ.push(d);
                        }
                        for o in 0..supers.len() {
                            if o != si {
                                out.push((supers[o].node, Lane::Unreliable, occ.clone()));
                            }
                        }
                    }
                }
                // State and minimap for a player wait for its island's next bundle.
                let interval = supers[si].interval as usize;
                let waits = |to: &usize, m: &[u8]| {
                    matches!(m[0], 3 | 5)
                        && player_of_node
                            .get(to)
                            .is_some_and(|&x| !(t as usize + players[x].island).is_multiple_of(interval))
                };
                let (hold, out): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut supers[si].held).into_iter().chain(out).partition(|(to, _, m)| waits(to, m));
                supers[si].held = hold;
                for (to, lane, m) in out {
                    sp_kind_bytes[m[0] as usize] += m.len() as u64;
                    sp_kind_msgs[m[0] as usize] += 1;
                    if sim.send(node, to, lane, &m).is_err() {
                        send_err += 1;
                    }
                }
            }
            let next = at(t)
                + Duration::from_micros((at(t + 1) - at(t)).as_micros() as u64 * (sub as u64 + 1) / a.substeps as u64);
            sim.run_for(next - sim.now);
        }
        // Players consume what arrived, with arrival times.
        for pi in 0..players.len() {
            let node = players[pi].node;
            for (when, e) in sim.take_timed_events(node) {
                let Event::Message { data, peer, .. } = e else { continue };
                match data[0] {
                    1 | 2 => {
                        let p = &mut players[pi];
                        if data[0] == 1 {
                            // An island member's INPUT, straight from them.
                            let pl = u16::from_le_bytes([data[1], data[2]]) as usize;
                            let tk = u32::from_le_bytes(data[3..7].try_into().unwrap());
                            let slot = island_of[pl].1;
                            take_runs(&data[13..], tk as usize, &mut p.known[slot]);
                            extend_contiguous(p, slot, ticks);
                        } else {
                            p.last_bundle = t;
                            // Bundles from another super-peer: the island moved there.
                            if let Some(&s) = node_to_super.get(&peer_to_node[&peer]) {
                                p.sup = s;
                            }
                            let base = u32::from_le_bytes(data[1..5].try_into().unwrap());
                            let d = (when - at(base)).as_secs_f64() * 1000.0;
                            p.delay_ms = if d > p.delay_ms { d } else { 0.95 * p.delay_ms + 0.05 * d };
                            p.delay_avg_ms = 0.95 * p.delay_avg_ms + 0.05 * d;
                            let mut b = &data[6..];
                            for _ in 0..data[5] {
                                let slot = b[0] as usize;
                                let tk = base - b[1] as u32;
                                let used = take_runs(&b[2..], tk as usize, &mut p.known[slot]);
                                extend_contiguous(p, slot, ticks);
                                b = &b[2 + used..];
                            }
                        }
                        let own = p.slot;
                        let c = p
                            .contiguous
                            .iter()
                            .enumerate()
                            .filter(|&(s, _)| s != own)
                            .map(|(_, &c)| c)
                            .min()
                            .unwrap_or(ticks);
                        let c = c.min(p.contiguous[own]);
                        for u in p.confirmed..c {
                            if u >= warmup {
                                let lag = (when - at(u)).as_secs_f64() * 1000.0;
                                lags.push(lag);
                                if kill_tick.is_some_and(|k| u >= k && u < k + 2 * a.hz) && orphaned.contains(&p.island)
                                {
                                    fail_lags.push(lag);
                                }
                            }
                        }
                        p.confirmed = p.confirmed.max(c);
                    }
                    3 => state_recv += 1,
                    5 => players[pi].deliver = data[3..].as_chunks::<3>().0.iter().map(|e| e[2]).collect(),
                    9 => {
                        let p = &mut players[pi];
                        let slot = data[1] as usize;
                        let end = u32::from_le_bytes(data[2..6].try_into().unwrap());
                        take_runs(&data[6..], end as usize, &mut p.known[slot]);
                        extend_contiguous(p, slot, ticks);
                        let own = p.slot;
                        let c = p
                            .contiguous
                            .iter()
                            .enumerate()
                            .filter(|&(s, _)| s != own)
                            .map(|(_, &c)| c)
                            .min()
                            .unwrap_or(ticks);
                        let c = c.min(p.contiguous[own]);
                        for u in p.confirmed..c {
                            if u >= warmup {
                                let lag = (when - at(u)).as_secs_f64() * 1000.0;
                                lags.push(lag);
                                if kill_tick.is_some_and(|k| u >= k && u < k + 2 * a.hz) && orphaned.contains(&p.island)
                                {
                                    fail_lags.push(lag);
                                }
                            }
                        }
                        p.confirmed = p.confirmed.max(c);
                    }
                    4 => {
                        let emit = u32::from_le_bytes(data[3..7].try_into().unwrap());
                        let emitter = u16::from_le_bytes([data[11], data[12]]);
                        if !players[pi].seen_events.insert((emitter, emit)) {
                            continue;
                        }
                        proj_recv += 1;
                        let entry = u32::from_le_bytes(data[7..11].try_into().unwrap());
                        proj_lat.push((when - at(emit)).as_secs_f64() * 1000.0);
                        if when > at(entry) {
                            proj_late += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Lockstep consistency: every island member replays the island from the
    // same confirmed inputs and must reach the same state CRC.
    let mut agree = 0;
    let mut stalled = 0;
    for (k, members) in islands.iter().enumerate() {
        let common = members.iter().map(|&m| players[m].confirmed).min().unwrap();
        if common < ticks {
            stalled += 1;
            if std::env::var("ARENA_DEBUG").is_ok() {
                for &m in members {
                    let p = &players[m];
                    let gaps: Vec<(usize, u32)> =
                        p.contiguous.iter().cloned().enumerate().filter(|&(_, c)| c < ticks).collect();
                    println!(
                        "island {k} player {m} slot {} confirmed {} gaps (slot, first missing tick) {gaps:?}",
                        p.slot, p.confirmed
                    );
                    for &(slot, g) in &gaps {
                        let w: Vec<String> = (g.saturating_sub(10)..(g + 12).min(ticks))
                            .map(|t| if p.known[slot][t as usize] == UNKNOWN { "_".into() } else { "x".into() })
                            .collect();
                        println!("   slot {slot} ticks {}..: {}", g.saturating_sub(10), w.join(""));
                    }
                }
            }
        }
        let crcs: Vec<u16> = members
            .iter()
            .map(|&m| {
                let p = &players[m];
                let mut ents: Vec<PlayerEntityQ16> = members.iter().map(|&x| entities[x]).collect();
                for t in 0..common as usize {
                    for (slot, e) in ents.iter_mut().enumerate() {
                        e.step_physics(p.known[slot][t], q16_from_i32(ARENA as i32 - 1));
                    }
                }
                compute_island_state_crc16(common, &ents)
            })
            .collect();
        if crcs.iter().all(|&c| c == crcs[0]) {
            agree += 1;
        } else {
            println!("island {k}: CRC mismatch {crcs:?}");
        }
    }

    let secs = (ticks + a.hz) as f64 / a.hz as f64;
    let kbps = |b: u64| b as f64 * 8.0 / secs / 1000.0;
    let pl_up: Vec<f64> = players.iter().map(|p| kbps(sim.nodes[p.node].tx_bytes)).collect();
    let pl_down: Vec<f64> = players.iter().map(|p| kbps(sim.nodes[p.node].rx_bytes)).collect();
    let sp_up: Vec<f64> = supers.iter().map(|s| kbps(sim.nodes[s.node].tx_bytes) / 1000.0).collect();
    let sp_down: Vec<f64> = supers.iter().map(|s| kbps(sim.nodes[s.node].rx_bytes) / 1000.0).collect();
    let sp_pps: Vec<f64> = supers.iter().map(|s| sim.nodes[s.node].tx_packets as f64 / secs).collect();
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let max = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max);
    let (l50, l99, lmax) = (pctl(&mut lags, 0.5), pctl(&mut lags, 0.99), pctl(&mut lags, 1.0));
    println!(
        "net: {} ms one-way + 0..{} ms jitter, {:.1}% loss; player uplink {} kbps, super-peer uplink {} Mbps; {} Hz, redundancy {}",
        a.latency_ms, a.jitter_ms, a.loss * 100.0, a.player_up_kbps, a.super_up_mbps, a.hz, a.redundancy
    );
    println!(
        "confirm lag (input generated -> whole island's input known): p50 {l50:.1} ms ({:.1} ticks), p99 {l99:.1} ms ({:.1} ticks), max {lmax:.1} ms; {} samples",
        l50 / tick_ms,
        l99 / tick_ms,
        lags.len()
    );
    println!(
        "projectiles: {proj_recv}/{proj_sent} delivered, p50 {:.1} ms, p99 {:.1} ms; landing budget avg {:.1} ticks, late {:.2}%",
        pctl(&mut proj_lat, 0.5),
        pctl(&mut proj_lat, 0.99),
        flight_sum as f64 / proj_emits.max(1) as f64,
        100.0 * proj_late as f64 / proj_recv.max(1) as f64
    );
    if a.adapt {
        let tot = interval_ticks.iter().sum::<u64>().max(1) as f64;
        let shares: Vec<String> =
            interval_ticks[1..].iter().map(|&n| format!("{:.0}", 100.0 * n as f64 / tot)).collect();
        println!(
            "overload control: bundles every 1..{MAX_INTERVAL} ticks {}% of super-peer ticks, {interval_changes} changes",
            shares.join("/")
        );
    }
    if a.direct {
        println!(
            "island mesh: {}/{} member pairs direct ({:.0}%)",
            direct_pairs.0,
            direct_pairs.1,
            100.0 * direct_pairs.0 as f64 / direct_pairs.1.max(1) as f64
        );
    }
    if let Some(s) = a.kill {
        let moved: Vec<u32> =
            orphaned.iter().filter_map(|k| moved_at.get(k)).map(|&m| m.saturating_sub(kill_tick.unwrap())).collect();
        let new_owners: std::collections::BTreeSet<usize> = orphaned
            .iter()
            .map(|&k| supers.iter().position(|sp| !sp.dead && sp.islands.contains(&k)).unwrap_or(usize::MAX))
            .collect();
        println!(
            "failover: super-peer {s} killed at {} s; {}/{} of its islands moved (to super-peers {:?}), detected after {:.0}..{:.0} ms; {migrations} player migrations, {event_resends} events resent",
            a.kill_at,
            moved.len(),
            orphaned.len(),
            new_owners,
            moved.iter().min().map_or(f64::NAN, |&x| x as f64 * tick_ms),
            moved.iter().max().map_or(f64::NAN, |&x| x as f64 * tick_ms),
        );
        println!(
            "failover: confirm lag of its players' ticks in the 2 s after: p50 {:.0} ms, p99 {:.0} ms, max {:.0} ms",
            pctl(&mut fail_lags, 0.5),
            pctl(&mut fail_lags, 0.99),
            pctl(&mut fail_lags, 1.0)
        );
    }
    println!(
        "player: up avg {:.0} kbps (max {:.0}), down avg {:.0} kbps (max {:.0})",
        avg(&pl_up),
        max(&pl_up),
        avg(&pl_down),
        max(&pl_down)
    );
    println!(
        "super-peer: up avg {:.2} Mbps (max {:.2}), down avg {:.2} Mbps, {:.0} packets/s out (max {:.0}), {:.2}% dropped at a full uplink",
        avg(&sp_up),
        max(&sp_up),
        avg(&sp_down),
        avg(&sp_pps),
        max(&sp_pps),
        100.0 * supers.iter().map(|s| sim.nodes[s.node].uplink_dropped).sum::<u64>() as f64
            / supers.iter().map(|s| sim.nodes[s.node].tx_packets + sim.nodes[s.node].uplink_dropped).sum::<u64>().max(1) as f64
    );
    println!(
        "lockstep: {agree}/{n_islands} islands agree on state CRC, {stalled} stalled; {nacks} NACKs, {repairs} repairs; state msgs {state_recv}; send errors {send_err}; sim lost {} of {} packets",
        sim.stats.lost, sim.stats.sent
    );
    if std::env::var("ARENA_DEBUG").is_ok() {
        let sum = |nodes: &mut dyn Iterator<Item = usize>| {
            let mut t = peerwire_core::Stats::default();
            for n in nodes {
                let s = sim.nodes[n].endpoint.stats();
                t.rx_replayed += s.rx_replayed;
                t.rx_auth_failed += s.rx_auth_failed;
                t.rx_unknown_session += s.rx_unknown_session;
                t.rx_malformed += s.rx_malformed;
                t.unreliable_dropped += s.unreliable_dropped;
                t.retransmits += s.retransmits;
                t.roams += s.roams;
            }
            t
        };
        let mut ns = peerwire_sim::NatStats::default();
        for n in 0..a.players {
            let x = sim.nat_stats(n);
            ns.inbound_filtered += x.inbound_filtered;
            ns.inbound_no_mapping += x.inbound_no_mapping;
            ns.mappings_created += x.mappings_created;
        }
        println!("player NATs: {ns:?}");
        let names = ["?", "input", "bundle", "state", "proj", "minimap", "occupancy", "fwd_state", "nack", "repair"];
        for k in 0..10 {
            if sp_kind_msgs[k] > 0 {
                println!(
                    "super-peer out {:>9}: {:>8} msgs, {:.2} Mbps payload",
                    names[k],
                    sp_kind_msgs[k],
                    sp_kind_bytes[k] as f64 * 8.0 / secs / 1e6 / a.supers as f64
                );
            }
        }
        println!("players: {:?}", sum(&mut players.iter().map(|p| p.node)));
        println!("supers: {:?}", sum(&mut supers.iter().map(|s| s.node)));
    }
    println!("wall time {:.1} s for {:.0} s of game", wall.elapsed().as_secs_f64(), secs);
    Summary {
        islands: n_islands,
        agree,
        stalled,
        lag_p50_ms: l50,
        lag_p99_ms: l99,
        player_down_kbps: avg(&pl_down),
        super_up_mbps: avg(&sp_up),
    }
}
