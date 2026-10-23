//! C ABI over [`peerwire_core::Endpoint`], addressed by `u32` transport handles.
//!
//! One surface serves every host: built for `wasm32-unknown-unknown` it is a
//! WebAssembly module with no imports (browsers, via `packages/js/src/peerwire.js`); built
//! natively it is a shared or static library for Dart FFI, Swift or C.
//!
//! Conventions:
//! - Times are microseconds on the host's monotonic clock.
//! - Peer ids are 32-byte buffers.
//! - Buffers are allocated in this module's memory with [`pw_alloc`] and freed
//!   with [`pw_free`].
//! - Return codes: `>= 0` success (a length where one is returned), `< 0` an
//!   error from [`PW_ERR_*`](PW_ERR_NOT_CONNECTED).
//!
//! Events are encoded as `kind(u8) peer(32) ...`:
//! ```text
//! 1 CONNECTED      initiator(u8) path
//! 2 MESSAGE        lane(u8: 0 reliable, 1 unreliable) data...
//! 3 DISCONNECTED   reason(u8: 0 timeout, 1 closed, 2 replaced, 3 violation)
//! 4 CONNECT_FAILED
//! 5 PATH_CHANGED   path
//! 6 ADDR_OBSERVED  addr(u32)
//! path = 0 addr(u32) | 1 relay_peer(32)
//! ```

#![allow(clippy::missing_safety_doc)]

use peerwire_core::{Config, DisconnectReason, Endpoint, Event, Identity, Instant, Lane, Path, PeerId, SendError};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use std::slice;

pub const PW_ERR_NOT_CONNECTED: i32 = -1;
pub const PW_ERR_TOO_LARGE: i32 = -2;
pub const PW_ERR_WOULD_BLOCK: i32 = -3;
pub const PW_ERR_INVALID: i32 = -4;
/// The output buffer is too small; nothing was consumed.
pub const PW_ERR_BUFFER: i32 = -5;

pub struct PwEndpoint {
    ep: Endpoint<u32>,
    identity: Identity,
    pending: Option<Vec<u8>>,
    /// Randomness for sealed messages (a separate ChaCha stream from the endpoint's).
    rng: ChaCha20Rng,
}

fn now(us: u64) -> Instant {
    Instant::from_micros(us)
}

unsafe fn peer(p: *const u8) -> Option<PeerId> {
    if p.is_null() {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(slice::from_raw_parts(p, 32));
    Some(PeerId(id))
}

unsafe fn bytes<'a>(p: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        slice::from_raw_parts(p, len)
    }
}

#[no_mangle]
pub extern "C" fn pw_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[no_mangle]
pub unsafe extern "C" fn pw_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(Vec::from_raw_parts(p, 0, len.max(1)));
    }
}

/// Creates an endpoint. `secret` is a 32-byte Ed25519 secret key, or null to
/// derive a new identity from `seed`. `seed` must be 32 bytes from a CSPRNG
/// (`crypto.getRandomValues` in browsers). Writes the PeerId to `out_peer`.
/// `share_addresses` should be 0 when addresses are local handles.
#[no_mangle]
pub unsafe extern "C" fn pw_endpoint_new(
    secret: *const u8,
    seed: *const u8,
    share_addresses: u32,
    out_peer: *mut u8,
) -> *mut PwEndpoint {
    if seed.is_null() || out_peer.is_null() {
        return std::ptr::null_mut();
    }
    let mut s = [0u8; 32];
    s.copy_from_slice(bytes(seed, 32));
    let identity = if secret.is_null() {
        // Keep the endpoint's seed independent of the key-derivation stream.
        let mut rng = ChaCha20Rng::from_seed(s);
        let id = Identity::generate(&mut rng);
        rand_core::RngCore::fill_bytes(&mut rng, &mut s);
        id
    } else {
        let mut k = [0u8; 32];
        k.copy_from_slice(bytes(secret, 32));
        Identity::from_secret_bytes(&k)
    };
    let config = Config { share_addresses: share_addresses != 0, ..Config::default() };
    let ep = Endpoint::new(&identity, config, s);
    let mut rng = ChaCha20Rng::from_seed(s);
    rng.set_stream(1);
    slice::from_raw_parts_mut(out_peer, 32).copy_from_slice(ep.peer_id().as_bytes());
    Box::into_raw(Box::new(PwEndpoint { ep, identity, pending: None, rng }))
}

#[no_mangle]
pub unsafe extern "C" fn pw_endpoint_free(h: *mut PwEndpoint) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// Copies the 32-byte secret key to `out`, so the host can persist the identity.
#[no_mangle]
pub unsafe extern "C" fn pw_secret(h: *const PwEndpoint, out: *mut u8) {
    slice::from_raw_parts_mut(out, 32).copy_from_slice(&(*h).identity.secret_bytes());
}

/// Dials `peer_id` at `addr`. A no-op if already linked, or while relay-only if
/// its dial has a relay (the dial takes no new direct path); otherwise a dial
/// already running takes `addr` as one more path.
#[no_mangle]
pub unsafe extern "C" fn pw_connect(h: *mut PwEndpoint, now_us: u64, addr: u32, peer_id: *const u8) -> i32 {
    let Some(p) = peer(peer_id) else { return PW_ERR_INVALID };
    match (*h).ep.connect(now(now_us), addr, p) {
        Ok(()) => 0,
        Err(_) => PW_ERR_INVALID,
    }
}

/// Dials `peer_id` through the connected peer `relay`.
#[no_mangle]
pub unsafe extern "C" fn pw_connect_via(h: *mut PwEndpoint, now_us: u64, relay: *const u8, peer_id: *const u8) -> i32 {
    let (Some(r), Some(p)) = (peer(relay), peer(peer_id)) else { return PW_ERR_INVALID };
    match (*h).ep.connect_via(now(now_us), r, p, &[]) {
        Ok(()) => 0,
        Err(_) => PW_ERR_INVALID,
    }
}

/// Adds `addr` as a direct address of `peer_id`, a relayed or dialling peer: the
/// endpoint punches towards it and moves the link there once the peer answers
/// from it. Does nothing for a peer linked directly, an unknown one, or, while
/// relay-only, a peer linked through a relay or being dialled with one: nothing
/// is sent to `addr`.
#[no_mangle]
pub unsafe extern "C" fn pw_add_candidate(h: *mut PwEndpoint, now_us: u64, addr: u32, peer_id: *const u8) -> i32 {
    let Some(p) = peer(peer_id) else { return PW_ERR_INVALID };
    (*h).ep.add_candidates(now(now_us), &p, &[addr]);
    0
}

/// Relay-only (`on` != 0): relayed links stay relayed (no punching, no direct
/// path), dials through a relay alone (given the relay before any direct address
/// of their own, or with a relay when this is turned on) send nothing direct,
/// relays are asked not to tag our packets with our address, and a direct
/// handshake is answered only from a peer this endpoint is dialling directly. A
/// direct dial later given a relay keeps its direct addresses and may complete
/// direct.
#[no_mangle]
pub unsafe extern "C" fn pw_set_relay_only(h: *mut PwEndpoint, on: u32) {
    (*h).ep.set_relay_only(on != 0);
}

#[no_mangle]
pub unsafe extern "C" fn pw_disconnect(h: *mut PwEndpoint, peer_id: *const u8) {
    if let Some(p) = peer(peer_id) {
        (*h).ep.disconnect(&p);
    }
}

#[no_mangle]
pub unsafe extern "C" fn pw_is_connected(h: *const PwEndpoint, peer_id: *const u8) -> u32 {
    peer(peer_id).is_some_and(|p| (*h).ep.is_connected(&p)) as u32
}

/// Smoothed RTT in microseconds, or -1 if unknown.
#[no_mangle]
pub unsafe extern "C" fn pw_rtt_us(h: *const PwEndpoint, peer_id: *const u8) -> i64 {
    peer(peer_id).and_then(|p| (*h).ep.rtt(&p)).map_or(-1, |d| d.as_micros() as i64)
}

/// Writes 16 counters as u64 to `out`: tx_packets, rx_packets, rx_malformed,
/// rx_unknown_session, rx_auth_failed, rx_replayed, rx_handshake_dropped,
/// retransmits, unreliable_dropped, roams, relayed, relay_dropped,
/// punches_sent, punch_succeeded, punch_failed, direct_fallbacks.
#[no_mangle]
pub unsafe extern "C" fn pw_stats(h: *const PwEndpoint, out: *mut u64) {
    let s = (*h).ep.stats();
    let v = [
        s.tx_packets,
        s.rx_packets,
        s.rx_malformed,
        s.rx_unknown_session,
        s.rx_auth_failed,
        s.rx_replayed,
        s.rx_handshake_dropped,
        s.retransmits,
        s.unreliable_dropped,
        s.roams,
        s.relayed,
        s.relay_dropped,
        s.punches_sent,
        s.punch_succeeded,
        s.punch_failed,
        s.direct_fallbacks,
    ];
    slice::from_raw_parts_mut(out, 16).copy_from_slice(&v);
}

/// `lane`: 0 reliable (up to 1 MiB), 1 unreliable (up to 1100 B), 2 urgent
/// (reliable, each fragment sent twice, up to 64 KiB). Larger is PW_ERR_TOO_LARGE.
#[no_mangle]
pub unsafe extern "C" fn pw_send(
    h: *mut PwEndpoint,
    peer_id: *const u8,
    lane: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    let Some(p) = peer(peer_id) else { return PW_ERR_INVALID };
    let lane = match lane {
        0 => Lane::Reliable,
        2 => Lane::Urgent,
        _ => Lane::Unreliable,
    };
    match (*h).ep.send(&p, lane, bytes(data, len)) {
        Ok(()) => 0,
        Err(SendError::NotConnected) => PW_ERR_NOT_CONNECTED,
        Err(SendError::TooLarge) => PW_ERR_TOO_LARGE,
        Err(SendError::WouldBlock) => PW_ERR_WOULD_BLOCK,
    }
}

#[no_mangle]
pub unsafe extern "C" fn pw_handle_datagram(h: *mut PwEndpoint, now_us: u64, addr: u32, data: *const u8, len: usize) {
    (*h).ep.handle_datagram(now(now_us), addr, bytes(data, len));
}

#[no_mangle]
pub unsafe extern "C" fn pw_handle_timeout(h: *mut PwEndpoint, now_us: u64) {
    (*h).ep.handle_timeout(now(now_us));
}

/// Next deadline in microseconds, or -1 if none.
#[no_mangle]
pub unsafe extern "C" fn pw_poll_timeout(h: *const PwEndpoint) -> i64 {
    (*h).ep.poll_timeout().map_or(-1, |t| t.as_micros() as i64)
}

/// Writes the next datagram into `buf` (at least `MAX_DATAGRAM` = 1232 bytes)
/// and its destination handle into `out_addr`. Returns its length, 0 if none.
#[no_mangle]
pub unsafe extern "C" fn pw_poll_transmit(
    h: *mut PwEndpoint,
    now_us: u64,
    out_addr: *mut u32,
    buf: *mut u8,
    cap: usize,
) -> i32 {
    if cap < peerwire_core::MAX_DATAGRAM {
        return PW_ERR_BUFFER;
    }
    match (*h).ep.poll_transmit(now(now_us)) {
        Some(t) => {
            *out_addr = t.dst;
            slice::from_raw_parts_mut(buf, t.payload.len()).copy_from_slice(&t.payload);
            t.payload.len() as i32
        }
        None => 0,
    }
}

fn put_path(v: &mut Vec<u8>, p: &Path<u32>) {
    match p {
        Path::Direct(a) => {
            v.push(0);
            v.extend_from_slice(&a.to_le_bytes());
        }
        Path::Via(r) => {
            v.push(1);
            v.extend_from_slice(r.as_bytes());
        }
    }
}

fn encode_event(e: Event<u32>) -> Vec<u8> {
    let mut v = Vec::new();
    let mut head = |kind: u8, p: &PeerId| {
        v.push(kind);
        v.extend_from_slice(p.as_bytes());
    };
    match e {
        Event::Connected { peer, path, initiator } => {
            head(1, &peer);
            v.push(initiator as u8);
            put_path(&mut v, &path);
        }
        Event::Message { peer, lane, data } => {
            head(2, &peer);
            v.push(matches!(lane, Lane::Unreliable) as u8);
            v.extend_from_slice(&data);
        }
        Event::Disconnected { peer, reason } => {
            head(3, &peer);
            v.push(match reason {
                DisconnectReason::Timeout => 0,
                DisconnectReason::ClosedByPeer => 1,
                DisconnectReason::Replaced => 2,
                DisconnectReason::ProtocolViolation => 3,
            });
        }
        Event::ConnectFailed { peer } => head(4, &peer),
        Event::PathChanged { peer, path } => {
            head(5, &peer);
            put_path(&mut v, &path);
        }
        Event::AddrObserved { by, addr } => {
            head(6, &by);
            v.extend_from_slice(&addr.to_le_bytes());
        }
    }
    v
}

/// Writes the next event into `buf`. Returns its length, 0 if none, or
/// `-(needed)` if `buf` is too small (the event stays queued).
#[no_mangle]
pub unsafe extern "C" fn pw_poll_event(h: *mut PwEndpoint, buf: *mut u8, cap: usize) -> i32 {
    let h = &mut *h;
    let e = match h.pending.take() {
        Some(e) => e,
        None => match h.ep.poll_event() {
            Some(e) => encode_event(e),
            None => return 0,
        },
    };
    if e.len() > cap {
        let n = e.len() as i32;
        h.pending = Some(e);
        return -n;
    }
    slice::from_raw_parts_mut(buf, e.len()).copy_from_slice(&e);
    e.len() as i32
}

/// Seals `len` bytes to peer `to` (see `peerwire_core::sealed`). Returns the sealed
/// length, or `-(needed)` if `cap` is too small, or PW_ERR_TOO_LARGE.
#[no_mangle]
pub unsafe extern "C" fn pw_seal(
    h: *mut PwEndpoint,
    to: *const u8,
    data: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> i32 {
    let Some(to) = peer(to) else { return PW_ERR_INVALID };
    let needed = len + peerwire_core::sealed::SEAL_OVERHEAD;
    if len > peerwire_core::sealed::MAX_SEALED {
        return PW_ERR_TOO_LARGE;
    }
    if cap < needed {
        return -(needed as i32);
    }
    let h = &mut *h;
    let mut seed = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut h.rng, &mut seed);
    match peerwire_core::sealed::seal(&h.identity, &to, seed, bytes(data, len)) {
        Some(s) => {
            slice::from_raw_parts_mut(out, s.len()).copy_from_slice(&s);
            s.len() as i32
        }
        None => PW_ERR_INVALID,
    }
}

/// Opens a message sealed to this endpoint. Writes the sender's PeerId to
/// `out_from` and the plaintext to `out` (at least `len` bytes). Returns the
/// plaintext length, or PW_ERR_INVALID if it isn't for us or was tampered with.
#[no_mangle]
pub unsafe extern "C" fn pw_open(
    h: *const PwEndpoint,
    data: *const u8,
    len: usize,
    out_from: *mut u8,
    out: *mut u8,
    cap: usize,
) -> i32 {
    if cap < len {
        return PW_ERR_BUFFER;
    }
    match peerwire_core::sealed::open(&(*h).identity, bytes(data, len)) {
        Some((from, pt)) => {
            slice::from_raw_parts_mut(out_from, 32).copy_from_slice(from.as_bytes());
            slice::from_raw_parts_mut(out, pt.len()).copy_from_slice(&pt);
            pt.len() as i32
        }
        None => PW_ERR_INVALID,
    }
}

/// Derives a secp256k1 key for Nostr from 32 seed bytes (a fresh key per
/// session is the intended use). Writes the secret and the x-only public key.
/// Returns 0, or PW_ERR_INVALID if the seed isn't a valid scalar (retry).
#[no_mangle]
pub unsafe extern "C" fn pw_nostr_keypair(seed: *const u8, out_secret: *mut u8, out_pub: *mut u8) -> i32 {
    let Ok(sk) = k256::schnorr::SigningKey::from_bytes(bytes(seed, 32)) else { return PW_ERR_INVALID };
    slice::from_raw_parts_mut(out_secret, 32).copy_from_slice(&sk.to_bytes());
    slice::from_raw_parts_mut(out_pub, 32).copy_from_slice(&sk.verifying_key().to_bytes());
    0
}

/// BIP-340 Schnorr signature over a 32-byte message (a Nostr event id), with
/// 32 bytes of auxiliary randomness.
#[no_mangle]
pub unsafe extern "C" fn pw_nostr_sign(secret: *const u8, msg: *const u8, aux: *const u8, out_sig: *mut u8) -> i32 {
    let Ok(sk) = k256::schnorr::SigningKey::from_bytes(bytes(secret, 32)) else { return PW_ERR_INVALID };
    let mut a = [0u8; 32];
    a.copy_from_slice(bytes(aux, 32));
    let Ok(sig) = sk.sign_raw(bytes(msg, 32), &a) else { return PW_ERR_INVALID };
    slice::from_raw_parts_mut(out_sig, 64).copy_from_slice(&sig.to_bytes());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn pump(a: *mut PwEndpoint, b: *mut PwEndpoint, t: u64) {
        let mut buf = vec![0u8; 2048];
        let mut addr = 0u32;
        loop {
            let mut moved = false;
            for (from, to, from_addr) in [(a, b, 1u32), (b, a, 2u32)] {
                loop {
                    let n = pw_poll_transmit(from, t, &mut addr, buf.as_mut_ptr(), buf.len());
                    if n <= 0 {
                        break;
                    }
                    pw_handle_datagram(to, t, from_addr, buf.as_ptr(), n as usize);
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }

    #[test]
    fn two_endpoints_through_the_c_abi() {
        unsafe {
            let (mut ida, mut idb) = ([0u8; 32], [0u8; 32]);
            let a = pw_endpoint_new(std::ptr::null(), [1u8; 32].as_ptr(), 0, ida.as_mut_ptr());
            let b = pw_endpoint_new(std::ptr::null(), [2u8; 32].as_ptr(), 0, idb.as_mut_ptr());
            // a reaches b at handle 2, b reaches a at handle 1
            assert_eq!(pw_connect(a, 0, 2, idb.as_ptr()), 0);
            pump(a, b, 1000);
            assert_eq!(pw_is_connected(a, idb.as_ptr()), 1);
            assert_eq!(pw_is_connected(b, ida.as_ptr()), 1);
            let msg = vec![9u8; 5000];
            assert_eq!(pw_send(a, idb.as_ptr(), 0, msg.as_ptr(), msg.len()), 0);
            pump(a, b, 2000);
            let mut small = [0u8; 16];
            let mut got = Vec::new();
            loop {
                let n = pw_poll_event(b, small.as_mut_ptr(), small.len());
                if n == 0 {
                    break;
                }
                if n < 0 {
                    let mut big = vec![0u8; (-n) as usize];
                    assert_eq!(pw_poll_event(b, big.as_mut_ptr(), big.len()), -n);
                    got.push(big);
                } else {
                    got.push(small[..n as usize].to_vec());
                }
            }
            assert_eq!(got[0][0], 1, "connected first");
            let m = got.iter().find(|e| e[0] == 2).expect("message");
            assert_eq!(&m[1..33], &ida);
            assert_eq!(m[33], 0);
            assert_eq!(&m[34..], &msg[..]);
            let mut secret = [0u8; 32];
            pw_secret(a, secret.as_mut_ptr());
            let mut again = [0u8; 32];
            let c = pw_endpoint_new(secret.as_ptr(), [3u8; 32].as_ptr(), 0, again.as_mut_ptr());
            assert_eq!(again, ida, "identity restored from the secret");
            // Sealed round trip through the ABI.
            let mut sealed = vec![0u8; 256];
            let n = pw_seal(a, idb.as_ptr(), b"sdp".as_ptr(), 3, sealed.as_mut_ptr(), sealed.len());
            assert!(n > 0);
            let (mut from, mut pt) = ([0u8; 32], vec![0u8; 256]);
            assert_eq!(pw_open(b, sealed.as_ptr(), n as usize, from.as_mut_ptr(), pt.as_mut_ptr(), pt.len()), 3);
            assert_eq!((&from, &pt[..3]), (&ida, &b"sdp"[..]));
            assert_eq!(
                pw_open(c, sealed.as_ptr(), n as usize, from.as_mut_ptr(), pt.as_mut_ptr(), pt.len()),
                PW_ERR_INVALID
            );
            // Nostr keys sign verifiably.
            let (mut nsk, mut npk, mut sig) = ([0u8; 32], [0u8; 32], [0u8; 64]);
            assert_eq!(pw_nostr_keypair([5u8; 32].as_ptr(), nsk.as_mut_ptr(), npk.as_mut_ptr()), 0);
            assert_eq!(pw_nostr_sign(nsk.as_ptr(), [9u8; 32].as_ptr(), [1u8; 32].as_ptr(), sig.as_mut_ptr()), 0);
            let vk = k256::schnorr::VerifyingKey::from_bytes(&npk).unwrap();
            let s = k256::schnorr::Signature::try_from(&sig[..]).unwrap();
            assert!(vk.verify_raw(&[9u8; 32], &s).is_ok());
            for h in [a, b, c] {
                pw_endpoint_free(h);
            }
        }
    }

    /// An endpoint's index and a handle it uses.
    type Hop = (usize, u32);

    /// Delivers every queued datagram along `routes`: (from endpoint, its handle)
    /// → (to endpoint, the handle `to` sees it at). Datagrams to other handles are
    /// lost. Returns where each datagram was sent, delivered or not.
    unsafe fn route(eps: &[*mut PwEndpoint], routes: &[(Hop, Hop)], t: u64) -> Vec<Hop> {
        let (mut buf, mut addr, mut sent) = (vec![0u8; 2048], 0u32, Vec::new());
        loop {
            let mut moved = false;
            for (i, &h) in eps.iter().enumerate() {
                loop {
                    let n = pw_poll_transmit(h, t, &mut addr, buf.as_mut_ptr(), buf.len());
                    if n <= 0 {
                        break;
                    }
                    sent.push((i, addr));
                    if let Some(&(_, (to, seen))) = routes.iter().find(|(from, _)| *from == (i, addr)) {
                        pw_handle_datagram(eps[to], t, seen, buf.as_ptr(), n as usize);
                        moved = true;
                    }
                }
            }
            if !moved {
                return sent;
            }
        }
    }

    /// The direct handles `h` reported in PATH_CHANGED events since the last call.
    unsafe fn direct_paths(h: *mut PwEndpoint) -> Vec<u32> {
        let (mut buf, mut out) = (vec![0u8; 4096], Vec::new());
        loop {
            let n = pw_poll_event(h, buf.as_mut_ptr(), buf.len());
            if n <= 0 {
                return out;
            }
            if buf[0] == 5 && buf[33] == 0 {
                out.push(u32::from_le_bytes(buf[34..38].try_into().unwrap()));
            }
        }
    }

    /// pw_add_candidate: a link a and b made through relay r moves to the direct
    /// handle once one side adds it (both sides end up there), and stays relayed
    /// without it. A bad peer pointer is refused; an unknown peer changes nothing.
    #[test]
    fn add_candidate_moves_a_relayed_link_to_a_direct_handle() {
        unsafe {
            for add in [true, false] {
                let mut ids = [[0u8; 32]; 3];
                let eps: Vec<_> = (0..3)
                    .map(|i| pw_endpoint_new(std::ptr::null(), [i as u8 + 1; 32].as_ptr(), 0, ids[i].as_mut_ptr()))
                    .collect();
                let (a, b) = (eps[0], eps[1]);
                // a sees r at 10 and b at 11; b sees r at 20 and a at 21; r sees a at 30 and b at 31.
                let mut routes = vec![((0, 10), (2, 30)), ((1, 20), (2, 31)), ((2, 30), (0, 10)), ((2, 31), (1, 20))];
                let mut t = 1000u64;
                assert_eq!(pw_connect(a, t, 10, ids[2].as_ptr()), 0);
                assert_eq!(pw_connect(b, t, 20, ids[2].as_ptr()), 0);
                route(&eps, &routes, t);
                assert_eq!(pw_connect_via(a, t, ids[2].as_ptr(), ids[1].as_ptr()), 0);
                route(&eps, &routes, t);
                assert_eq!((pw_is_connected(a, ids[1].as_ptr()), pw_is_connected(b, ids[0].as_ptr())), (1, 1));
                // The direct path between a and b opens only now (a birthday punch's channel, say).
                routes.extend([((0, 11), (1, 21)), ((1, 21), (0, 11))]);
                direct_paths(a); // drop the events so far
                direct_paths(b);
                if add {
                    assert_eq!(pw_add_candidate(a, t, 11, std::ptr::null()), PW_ERR_INVALID);
                    assert_eq!(pw_add_candidate(a, t, 11, [7u8; 32].as_ptr()), 0, "an unknown peer is a no-op");
                    assert_eq!(pw_add_candidate(a, t, 11, ids[1].as_ptr()), 0);
                }
                let (mut pa, mut pb) = (Vec::new(), Vec::new());
                for _ in 0..40 {
                    t += 50_000;
                    for &h in &eps {
                        pw_handle_timeout(h, t);
                    }
                    route(&eps, &routes, t);
                    pa.extend(direct_paths(a));
                    pb.extend(direct_paths(b));
                }
                if add {
                    assert_eq!((pa.last(), pb.last()), (Some(&11), Some(&21)), "both sides moved to the direct handle");
                } else {
                    assert!(pa.is_empty() && pb.is_empty(), "no candidate, no move: {pa:?} {pb:?}");
                }
                assert_eq!((pw_is_connected(a, ids[1].as_ptr()), pw_is_connected(b, ids[0].as_ptr())), (1, 1));
                for h in eps {
                    pw_endpoint_free(h);
                }
            }
        }
    }

    /// NB48b-4: relay-only, a dial through relay r sends nothing to a direct
    /// address of the peer added while it is pending (pw_add_candidate, or
    /// pw_connect) or once the link is up, though that path would work: the link
    /// comes up through r and stays there.
    #[test]
    fn relay_only_dial_through_a_relay_sends_nothing_direct() {
        unsafe {
            let mut ids = [[0u8; 32]; 3];
            let eps: Vec<_> = (0..3)
                .map(|i| pw_endpoint_new(std::ptr::null(), [i as u8 + 1; 32].as_ptr(), 0, ids[i].as_mut_ptr()))
                .collect();
            let (a, b) = (eps[0], eps[1]);
            // As above (a sees r at 10, b at 11; ...), with the direct path open from the start.
            let routes = [
                ((0, 10), (2, 30)),
                ((1, 20), (2, 31)),
                ((2, 30), (0, 10)),
                ((2, 31), (1, 20)),
                ((0, 11), (1, 21)),
                ((1, 21), (0, 11)),
            ];
            let mut t = 1000u64;
            assert_eq!(pw_connect(a, t, 10, ids[2].as_ptr()), 0);
            assert_eq!(pw_connect(b, t, 20, ids[2].as_ptr()), 0);
            route(&eps, &routes, t);
            pw_set_relay_only(a, 1);
            assert_eq!(pw_connect_via(a, t, ids[2].as_ptr(), ids[1].as_ptr()), 0);
            let mut events = Vec::new();
            for live in [false, true] {
                // First while the dial is pending (nothing routed yet), then on the live link.
                assert_eq!(pw_add_candidate(a, t, 11, ids[1].as_ptr()), 0);
                assert_eq!(pw_connect(a, t, 11, ids[1].as_ptr()), 0);
                let mut sent = Vec::new();
                for _ in 0..40 {
                    sent.extend(route(&eps, &routes, t));
                    t += 50_000;
                    for &h in &eps {
                        pw_handle_timeout(h, t);
                    }
                }
                let direct = sent.iter().filter(|&&s| s == (0, 11)).count();
                assert_eq!(direct, 0, "live {live}: datagrams from a to b's direct handle");
                assert!(live || sent.contains(&(0, 10)), "the dial goes to r");
                assert_eq!((pw_is_connected(a, ids[1].as_ptr()), pw_is_connected(b, ids[0].as_ptr())), (1, 1));
                let mut buf = vec![0u8; 4096];
                loop {
                    let n = pw_poll_event(a, buf.as_mut_ptr(), buf.len());
                    if n <= 0 {
                        break;
                    }
                    events.push(buf[..n as usize].to_vec());
                }
            }
            // a's link to b came up through r and never moved.
            let to_b: Vec<_> = events.iter().filter(|e| e[1..33] == ids[1]).collect();
            assert_eq!(to_b.len(), 1, "one event for b: {to_b:?}");
            assert_eq!((to_b[0][0], &to_b[0][34..]), (1, &[&[1u8][..], &ids[2]].concat()[..]), "connected via r");
            for h in eps {
                pw_endpoint_free(h);
            }
        }
    }

    /// Whether `e` has the documented event encoding (see the module docs).
    fn event_ok(e: &[u8]) -> bool {
        let path_ok = |p: &[u8]| matches!(p, [0, _, _, _, _]) || p.len() == 33 && p[0] == 1;
        e.len() >= 33
            && match e[0] {
                1 => e.len() >= 35 && e[33] <= 1 && path_ok(&e[34..]),
                2 => e.len() >= 34 && e[33] <= 1,
                3 => e.len() == 34 && e[33] <= 3,
                4 => e.len() == 33,
                5 => path_ok(&e[33..]),
                6 => e.len() == 37,
                _ => false,
            }
    }

    /// NB-014: two endpoints driven only through the C ABI by random calls with
    /// random arguments (lanes, lengths, buffer sizes, peers), random and
    /// mutated datagrams, and random sealed blobs. Every return code is one the
    /// ABI documents, a too-small event buffer keeps the event for a retry, and
    /// every event has the documented encoding (what peerwire.js decodes).
    #[test]
    fn fuzz_the_c_abi() {
        use peerwire_sim::fuzz::{cases, mutate, Rng};
        unsafe {
            let mut ids = [[0u8; 32]; 2];
            let eps = [
                pw_endpoint_new(std::ptr::null(), [1u8; 32].as_ptr(), 0, ids[0].as_mut_ptr()),
                pw_endpoint_new(std::ptr::null(), [2u8; 32].as_ptr(), 0, ids[1].as_mut_ptr()),
            ];
            let mut rng = Rng::for_target("ffi");
            let (mut t, mut captured, mut events) = (1000u64, Vec::<Vec<u8>>::new(), 0usize);
            let mut buf = vec![0u8; 1 << 21];
            assert_eq!(pw_connect(eps[0], t, 2, ids[1].as_ptr()), 0);
            for i in 0..cases(20_000) {
                let x = rng.below(2);
                let (h, other) = (eps[x], ids[1 - x]);
                let target: [u8; 32] = match rng.below(4) {
                    0 => rng.bytes(32).try_into().unwrap(),
                    1 => ids[x],
                    _ => other,
                };
                match rng.below(9) {
                    0 => {
                        let any = rng.below(3000);
                        let n = *rng.pick(&[0, 1, 1100, 1101, 1232, 5000, any]);
                        let lane = rng.below(4) as u32;
                        let r = pw_send(h, target.as_ptr(), lane, buf.as_ptr(), n);
                        assert!([0, PW_ERR_NOT_CONNECTED, PW_ERR_TOO_LARGE, PW_ERR_WOULD_BLOCK].contains(&r), "{r}");
                        if lane != 0 && lane != 2 && n > 1100 && r != PW_ERR_NOT_CONNECTED {
                            assert_eq!(r, PW_ERR_TOO_LARGE);
                        }
                    }
                    1 => {
                        let mut d = match captured.is_empty() || rng.one_in(4) {
                            true => {
                                let n = rng.below(1300);
                                rng.bytes(n)
                            }
                            false => rng.pick(&captured).clone(),
                        };
                        if rng.below(3) != 0 {
                            mutate(&mut rng, &mut d, 1400);
                        }
                        let any = rng.u32();
                        let from = *rng.pick(&[1, 2, 3, any]);
                        pw_handle_datagram(h, t, from, d.as_ptr(), d.len());
                    }
                    2 | 3 => {
                        let any = rng.below(100);
                        let cap = *rng.pick(&[0, 1, 33, 34, 40, 2048, any]);
                        let e = match pw_poll_event(h, buf.as_mut_ptr(), cap) {
                            0 => continue,
                            r if r > 0 => {
                                assert!(r as usize <= cap);
                                buf[..r as usize].to_vec()
                            }
                            r => {
                                let need = (-r) as usize;
                                assert!(need > cap, "case {i}: asked for {need} with {cap}");
                                assert_eq!(pw_poll_event(h, buf.as_mut_ptr(), need), need as i32, "kept for a retry");
                                buf[..need].to_vec()
                            }
                        };
                        assert!(event_ok(&e), "case {i}: bad event {e:?}");
                        events += 1;
                    }
                    4 => {
                        let cap = *rng.pick(&[0, 1231, 1232, 1500]);
                        let mut addr = 0u32;
                        for _ in 0..64 {
                            let r = pw_poll_transmit(h, t, &mut addr, buf.as_mut_ptr(), cap);
                            if r == PW_ERR_BUFFER {
                                assert!(cap < peerwire_core::MAX_DATAGRAM);
                                break;
                            }
                            if r == 0 {
                                break;
                            }
                            assert!(r > 0 && r as usize <= peerwire_core::MAX_DATAGRAM);
                            let d = buf[..r as usize].to_vec();
                            if addr == 1 || addr == 2 {
                                pw_handle_datagram(eps[addr as usize - 1], t, x as u32 + 1, d.as_ptr(), d.len());
                            }
                            if captured.len() < 256 {
                                captured.push(d);
                            }
                        }
                    }
                    5 => {
                        let n = rng.below(400);
                        let (mut out, cap) = (vec![0u8; 600], *rng.pick(&[0, 100, 600]));
                        let mut s = match pw_seal(h, target.as_ptr(), buf.as_ptr(), n, out.as_mut_ptr(), cap) {
                            r if r > 0 => out[..r as usize].to_vec(),
                            r => {
                                assert!(r == PW_ERR_INVALID || -r as usize > cap, "{r}");
                                rng.bytes(n)
                            }
                        };
                        if rng.one_in(2) {
                            mutate(&mut rng, &mut s, 700);
                        }
                        let (mut from, mut pt) = ([0u8; 32], vec![0u8; 700]);
                        let cap = *rng.pick(&[0, s.len(), 700]);
                        let r = pw_open(eps[1 - x], s.as_ptr(), s.len(), from.as_mut_ptr(), pt.as_mut_ptr(), cap);
                        assert!(
                            r == PW_ERR_BUFFER && cap < s.len()
                                || r == PW_ERR_INVALID
                                || r >= 0 && r as usize <= s.len()
                        );
                    }
                    6 => match rng.below(5) {
                        0 => pw_disconnect(h, target.as_ptr()),
                        1 => assert_eq!(pw_add_candidate(h, t, *rng.pick(&[1, 2, 3]), target.as_ptr()), 0),
                        _ => assert!([0, PW_ERR_INVALID].contains(&pw_connect(h, t, 2 - x as u32, target.as_ptr()))),
                    },
                    7 => {
                        let mut st = [0u64; 16];
                        pw_stats(h, st.as_mut_ptr());
                        let _ = (pw_rtt_us(h, target.as_ptr()), pw_is_connected(h, target.as_ptr()));
                        assert!(pw_poll_timeout(h) >= -1);
                    }
                    _ => {
                        let any = rng.below(3_000_000) as u64;
                        t += *rng.pick(&[0, 1_000, 30_000, any]);
                        pw_handle_timeout(h, t);
                    }
                }
            }
            println!("fuzz ffi: {events} events, all well-formed");
            assert!(events > 0);
            for h in eps {
                pw_endpoint_free(h);
            }
        }
    }
}

// The README examples run as doc tests (`cargo test --doc`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
