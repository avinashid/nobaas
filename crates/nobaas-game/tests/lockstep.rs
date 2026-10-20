//! Lockstep replay: peers that apply the same inputs compute bit-identical island state hashes on every tick.
//! This is the property a lockstep game (and the Godot binding, NB-020) relies on.

mod common;

use common::{range, rng, shuffle};
use nobaas_game::*;

const PLAYERS: usize = 30;
const TICKS: usize = 600; // 10 s at 60 Hz
const GRID: u32 = 256;

/// One peer's view: its own copy of every player, in whatever order it learned about them.
struct Peer {
    players: Vec<PlayerEntityQ16>,
}

impl Peer {
    fn new(order_salt: Option<u64>) -> Self {
        let mut players: Vec<PlayerEntityQ16> = (0..PLAYERS as u16)
            .map(|id| PlayerEntityQ16 {
                peer_id: id,
                pos: Vec2Q16::from_ints((id as i32 * 37) % GRID as i32, (id as i32 * 91) % GRID as i32),
                hp: 100,
                ..Default::default()
            })
            .collect();
        if let Some(salt) = order_salt {
            shuffle(&mut rng(salt), &mut players);
        }
        Peer { players }
    }

    /// Applies one tick of inputs (indexed by peer id) and returns `(island_id, crc)` for every island.
    fn tick(&mut self, tick: u32, inputs: &[u16]) -> Vec<(u16, u16)> {
        for p in &mut self.players {
            p.step_physics(inputs[p.peer_id as usize], q16_from_i32(GRID as i32 - 1));
        }
        partition_into_hilbert_islands(GRID, &self.players, MAX_ISLAND_PLAYERS)
            .iter()
            .map(|isl| (isl.island_id, compute_island_state_crc16(tick, &isl.players)))
            .collect()
    }
}

fn input_log() -> Vec<Vec<u16>> {
    let mut r = rng(30);
    (0..TICKS).map(|_| (0..PLAYERS).map(|_| range(&mut r, 0, 0x1f) as u16).collect()).collect()
}

fn replay(peer: &mut Peer, log: &[Vec<u16>]) -> Vec<Vec<(u16, u16)>> {
    log.iter().enumerate().map(|(t, inputs)| peer.tick(t as u32, inputs)).collect()
}

#[test]
fn same_inputs_give_bit_identical_hashes_every_tick() {
    let log = input_log();
    let want = replay(&mut Peer::new(None), &log);
    // Two more peers that registered players in different orders.
    for salt in [31, 32] {
        let mut peer = Peer::new(Some(salt));
        let got = replay(&mut peer, &log);
        for (t, (a, b)) in want.iter().zip(&got).enumerate() {
            assert_eq!(a, b, "tick {t}: peer with order salt {salt} diverged");
        }
    }
}

/// A fixed 600-tick run hashes to a fixed value on every platform and build. A change here means peers on the
/// old and new code would desync: treat it as a protocol change.
#[test]
fn replay_golden_hash() {
    let hashes = replay(&mut Peer::new(None), &input_log());
    let digest =
        hashes.iter().flatten().fold(0u64, |acc, &(id, crc)| acc.rotate_left(5) ^ ((id as u64) << 16 | crc as u64));
    assert_eq!(hashes[TICKS - 1].len(), PLAYERS.div_ceil(MAX_ISLAND_PLAYERS));
    assert_eq!(digest, GOLDEN_DIGEST, "digest {digest:#018x}");
}

const GOLDEN_DIGEST: u64 = 0x0a6c_0e28_e94a_c587;

/// One peer applies a single wrong input: its island's hash diverges on that tick, and the island quorum names
/// it while the other islands still agree.
#[test]
fn one_wrong_input_is_caught_by_quorum() {
    let log = input_log();
    let honest = replay(&mut Peer::new(None), &log);
    let cheat_tick = 300;
    let cheater: u16 = 7;
    let mut bad_log = log.clone();
    bad_log[cheat_tick][cheater as usize] ^= 0x10;
    let bad = replay(&mut Peer::new(Some(33)), &bad_log);

    assert_eq!(honest[..cheat_tick], bad[..cheat_tick], "identical before the bad input");
    let diverged: Vec<u16> =
        honest[cheat_tick].iter().zip(&bad[cheat_tick]).filter(|(a, b)| a != b).map(|(a, _)| a.0).collect();
    assert_eq!(diverged.len(), 1, "exactly the cheater's island diverges: {diverged:?}");

    // The island votes: every member but the cheater's machine reports the honest hash.
    let island = diverged[0] as usize;
    let members = 12u16;
    let mut votes: Vec<(u16, u16)> = (0..members).map(|m| (m, honest[cheat_tick][island].1)).collect();
    votes[cheater as usize % members as usize].1 = bad[cheat_tick][island].1;
    assert_eq!(find_byzantine_desync_peers(&votes), [cheater % members]);
}
