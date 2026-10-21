//! Reliable-ordered lane: fragmentation, selective ACK, RTO retransmission, reassembly.

use crate::time::Instant;
use crate::wire::{self, FRAGMENT, REL_OVERHEAD};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

const INITIAL_RTO: Duration = Duration::from_millis(300);
const MIN_RTO: Duration = Duration::from_millis(20);
const MAX_RTO: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Allowance for a receiver that acknowledges late (a busy browser event loop,
/// a game frame), as in QUIC's max_ack_delay. Loss is mostly repaired by fast
/// retransmit, so this only delays recovery of the last packets in a burst.
const ACK_DELAY: Duration = Duration::from_millis(25);
/// A fragment unacknowledged after this many sends means the link is dead.
const MAX_TRIES: u32 = 16;
/// Out-of-order fragments the receiver buffers ahead of the next expected one.
pub(crate) const RECV_WINDOW: u32 = 1024;
/// The urgent stream's receive window, which its sender also keeps the span of
/// fragments in flight under: the SACK bitmap's width, so every urgent
/// fragment in flight can be acknowledged, and room for a MAX_URGENT message.
/// With MAX_URGENT it bounds what a peer can make us hold for the urgent
/// stream to about 135 KB, against 2.2 MB for the reliable one.
pub(crate) const URGENT_RECV_WINDOW: u32 = 64;
/// Largest `Urgent` message (64 KiB), or `Config::max_message` if smaller.
pub const MAX_URGENT: usize = 64 << 10;
const _: () = assert!(MAX_URGENT.div_ceil(FRAGMENT) <= URGENT_RECV_WINDOW as usize);
/// Congestion window at the start, and its floor after loss (fragments).
const INITIAL_CWND: f64 = 16.0;
const MIN_CWND: f64 = 4.0;
/// A fragment this many sequence numbers below the highest acknowledged one is
/// presumed lost and resent without waiting for its timeout, once it is also
/// a quarter of the minimum RTT later than its ACK was expected (RACK's
/// reordering window, RFC 8985): a fragment merely overtaken costs nothing.
const DUP_THRESHOLD: u32 = 3;
/// Queueing delay (RTT above the minimum seen) at which the window backs off,
/// before any loss: keeps buffers short on paths that never drop (a browser
/// DataChannel, a bloated router), which real-time traffic needs.
const QUEUE_TARGET: Duration = Duration::from_millis(25);
/// Share of the fragments in flight the window keeps after a loss, and after
/// queueing delay over QUEUE_TARGET. Delay is the gentler signal: nothing was
/// dropped, and the queue drains within a round trip at 85%, whereas halving
/// a window near the cap costs the rest of the transfer (about one fragment
/// per RTT of regrowth).
const LOSS_CUT: f64 = 0.5;
const DELAY_CUT: f64 = 0.85;
/// A loss found by fast retransmit while the latest RTT sample showed under
/// this much queueing is taken as random (a lossy radio link, not a full
/// buffer) and keeps RANDOM_LOSS_CUT of the window instead of LOSS_CUT, as in
/// TCP Veno. A timeout always halves.
const RANDOM_LOSS_QUEUE: Duration = Duration::from_millis(5);
const RANDOM_LOSS_CUT: f64 = 0.8;
/// Tail-loss probe: when nothing has been acknowledged for srtt + 2·rttvar +
/// this, the newest unacknowledged fragment is sent again. Its ACK (or the SACK
/// gap it exposes) repairs a lost tail in about one RTT instead of an RTO;
/// sparse messages (a lone event) are always a tail. Much shorter than
/// ACK_DELAY: a late ACK only costs one duplicate fragment, not a window cut.
const PROBE_SLACK: Duration = Duration::from_millis(4);
/// An urgent fragment is sent a second time this much later, in its own
/// packet, so one loss (or a short burst) costs nothing.
pub(crate) const URGENT_GAP: Duration = Duration::from_millis(4);

struct Fragment {
    fin: bool,
    data: Vec<u8>,
    sent_at: Instant,
    due: Instant,
    tries: u32,
    /// Already scheduled for fast retransmit in this round.
    fast: bool,
    /// First sequence number sent after this fragment's latest transmission.
    /// Only an ACK at least DUP_THRESHOLD past it shows that copy was lost:
    /// ACKs for fragments already in flight when it went out say nothing about
    /// it, so each loss costs one fast retransmit, not one per ACK.
    epoch: u32,
    /// Sent again as a tail probe: its ACK can't be timed (Karn's rule).
    probed: bool,
    /// When to send the duplicate copy of the latest transmission (urgent only).
    dup_at: Option<Instant>,
}

/// Index of the reliable and the urgent stream in `RelSend::streams`.
const REL: usize = 0;
const URG: usize = 1;

/// One sequence space: reliable or urgent messages, each delivered in order.
struct Stream {
    next_seq: u32,
    /// One past the newest fragment sent so far.
    sent_end: u32,
    /// (seq, fin, data)
    queued: VecDeque<(u32, bool, Vec<u8>)>,
    inflight: BTreeMap<u32, Fragment>,
    /// `(due, seq)` and `(dup_at, seq)` of every in-flight fragment, so the
    /// earliest is found without walking `inflight` on each packet.
    by_due: BTreeSet<(Instant, u32)>,
    by_dup: BTreeSet<(Instant, u32)>,
    buffered: usize,
    /// Wire bytes of one copy of each fragment in flight (data and frame header).
    inflight_bytes: usize,
    /// When to send a tail-loss probe; one per flight, re-armed on ACK progress.
    probe_at: Option<Instant>,
    probe_used: bool,
    retransmits: u64,
    /// Fragments sent for the first time.
    fragments_sent: u64,
    probes: u64,
}

impl Stream {
    fn new() -> Self {
        Stream {
            next_seq: 0,
            sent_end: 0,
            queued: VecDeque::new(),
            inflight: BTreeMap::new(),
            by_due: BTreeSet::new(),
            by_dup: BTreeSet::new(),
            buffered: 0,
            inflight_bytes: 0,
            probe_at: None,
            probe_used: false,
            retransmits: 0,
            fragments_sent: 0,
            probes: 0,
        }
    }

    fn has_due(&self, now: Instant) -> bool {
        earliest(&self.by_due).is_some_and(|d| d <= now)
            || self.probe_at.is_some_and(|p| p <= now)
            || earliest(&self.by_dup).is_some_and(|d| d <= now)
    }
}

/// The sending side of a link's reliable and urgent streams. They have their
/// own sequence numbers, so an urgent message never waits for reliable data
/// queued before it, but one RTT estimate and one congestion window: urgent
/// fragments take free window slots first, up to half the window while
/// reliable data is waiting, so neither can starve the other. Urgent fragments
/// fill the window by wire bytes (both copies), as they are mostly small: 200
/// inputs a second take a few slots, not 20 per round trip at 100 ms.
pub(crate) struct RelSend {
    streams: [Stream; 2],
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    pub(crate) failed: bool,
    /// AIMD congestion window in slots (NewReno-style), for both streams.
    cwnd: f64,
    ssthresh: f64,
    /// No second reduction until this time (one per round trip).
    recover_until: Instant,
    min_rtt: Option<Duration>,
    path_min_rtt: Option<Duration>,
    /// Queueing delay of the latest RTT sample (sample − min_rtt); none before the first.
    queue_delay: Option<Duration>,
    pub(crate) delay_backoffs: u64,
    pub(crate) urgent_copies: u64,
}

pub(crate) enum PushError {
    BufferFull,
    TooLarge,
}

impl RelSend {
    pub(crate) fn new() -> Self {
        RelSend {
            streams: [Stream::new(), Stream::new()],
            srtt: None,
            rttvar: Duration::ZERO,
            rto: INITIAL_RTO,
            failed: false,
            cwnd: INITIAL_CWND,
            ssthresh: f64::INFINITY,
            recover_until: Instant::ZERO,
            min_rtt: None,
            path_min_rtt: None,
            queue_delay: None,
            delay_backoffs: 0,
            urgent_copies: 0,
        }
    }

    /// The congestion window, in slots, for both streams together.
    fn window(&self) -> usize {
        (self.cwnd as usize).max(MIN_CWND as usize)
    }

    /// Window slots the urgent fragments in flight take: a slot is a full fragment on the wire.
    fn urgent_slots(&self) -> usize {
        (2 * self.streams[URG].inflight_bytes).div_ceil(REL_OVERHEAD + FRAGMENT)
    }

    /// Window slots in use: reliable fragments, and urgent ones by size.
    fn slots(&self) -> usize {
        self.streams[REL].inflight.len() + self.urgent_slots()
    }

    /// Whether stream `u` may send a new fragment: the congestion window is
    /// shared, and urgent fragments fill at most half of it while reliable data
    /// waits. Each stream also stays within `max` fragments, the receiver's
    /// window, and urgent fragments within URGENT_RECV_WINDOW of the oldest one
    /// unacknowledged, since that receiver drops anything further ahead.
    fn may_send_new(&self, u: usize, max: usize) -> bool {
        let w = self.window();
        let st = &self.streams[u];
        !st.queued.is_empty()
            && st.inflight.len() < max.max(1)
            && (u == REL || st.inflight.first_key_value().is_none_or(|(&lo, _)| st.sent_end - lo < URGENT_RECV_WINDOW))
            && self.slots() < w
            && (u == REL || self.streams[REL].queued.is_empty() || self.urgent_slots() < w.div_ceil(2))
    }

    /// Cuts the window to `keep` of the fragments in flight, at most once per round trip.
    fn back_off(&mut self, now: Instant, keep: f64) {
        if now < self.recover_until {
            return;
        }
        self.ssthresh = (self.slots() as f64 * keep).max(MIN_CWND);
        self.cwnd = self.ssthresh;
        self.recover_until = now + self.srtt.unwrap_or(self.rto);
    }

    fn arm_probe(&mut self, u: usize, now: Instant) {
        let st = &mut self.streams[u];
        st.probe_at = match self.srtt {
            Some(srtt) if !st.probe_used && !st.inflight.is_empty() => {
                Some(now + srtt + (self.rttvar * 2).max(Duration::from_millis(1)) + PROBE_SLACK)
            }
            _ => None,
        };
    }

    pub(crate) fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    pub(crate) fn cwnd(&self) -> u32 {
        self.cwnd as u32
    }

    /// Window slots in use, comparable with `cwnd`: see `slots`.
    pub(crate) fn inflight(&self) -> u32 {
        self.slots() as u32
    }

    /// Fragments sent for the first time, both streams.
    pub(crate) fn fragments_sent(&self) -> u64 {
        self.streams.iter().map(|s| s.fragments_sent).sum()
    }

    /// Fragments sent again (timeouts, fast retransmits, tail probes), both streams.
    pub(crate) fn retransmits(&self) -> u64 {
        self.streams.iter().map(|s| s.retransmits).sum()
    }

    /// Queues a message on the urgent or the reliable stream; each has its own
    /// `max_buffer`, so a full reliable buffer never refuses an urgent message.
    pub(crate) fn push_message(
        &mut self,
        data: &[u8],
        max_buffer: usize,
        max_message: usize,
        urgent: bool,
    ) -> Result<(), PushError> {
        let st = &mut self.streams[urgent as usize];
        if data.len() > max_message {
            return Err(PushError::TooLarge);
        }
        if st.buffered + data.len() > max_buffer {
            return Err(PushError::BufferFull);
        }
        let count = data.len().div_ceil(FRAGMENT).max(1);
        if st.next_seq.checked_add(count as u32).is_none() {
            return Err(PushError::BufferFull);
        }
        for i in 0..count {
            let chunk = &data[(i * FRAGMENT).min(data.len())..((i + 1) * FRAGMENT).min(data.len())];
            st.queued.push_back((st.next_seq, i + 1 == count, chunk.to_vec()));
            st.next_seq += 1;
        }
        st.buffered += data.len();
        Ok(())
    }

    pub(crate) fn on_ack(&mut self, now: Instant, cum: u32, bits: u64, urgent: bool) {
        let u = urgent as usize;
        let st = &mut self.streams[u];
        let mut sample = None;
        let mut acked: Vec<u32> = st.inflight.range(..cum).map(|(&s, _)| s).collect();
        for i in 0..64u32 {
            if bits >> i & 1 == 1 {
                if let Some(s) = cum.checked_add(1 + i) {
                    acked.push(s);
                }
            }
        }
        let mut newly = 0usize;
        // Window slots acknowledged: the window grows by what the fragments took.
        let mut slots = 0.0;
        let mut highest = None;
        for s in acked {
            if let Some(f) = st.inflight.remove(&s) {
                st.by_due.remove(&(f.due, s));
                if let Some(d) = f.dup_at {
                    st.by_dup.remove(&(d, s));
                }
                st.buffered -= f.data.len();
                st.inflight_bytes -= REL_OVERHEAD + f.data.len();
                newly += 1;
                slots += match urgent {
                    true => (2 * (REL_OVERHEAD + f.data.len())) as f64 / (REL_OVERHEAD + FRAGMENT) as f64,
                    false => 1.0,
                };
                highest = highest.max(Some(s));
                if f.tries == 1 && !f.probed {
                    sample = Some(now - f.sent_at);
                }
            }
        }
        let mut queued_too_long = false;
        if let Some(r) = sample {
            self.update_rtt(r);
            let min = self.min_rtt.map_or(r, |m| m.min(r));
            self.min_rtt = Some(min);
            self.path_min_rtt = Some(self.path_min_rtt.map_or(r, |m| m.min(r)));
            queued_too_long = r > min + QUEUE_TARGET;
            self.queue_delay = Some(r - min);
        }
        if queued_too_long {
            if now >= self.recover_until {
                self.delay_backoffs += 1;
                self.back_off(now, DELAY_CUT);
            }
        } else if newly > 0 {
            if self.cwnd < self.ssthresh {
                self.cwnd += slots;
            } else {
                self.cwnd += slots / self.cwnd;
            }
            self.cwnd = self.cwnd.min(65536.0);
        }
        // Fast retransmit: fragments well below the highest acknowledged one,
        // after the reordering window. The window is cut when they are resent.
        if let Some(h) = highest {
            let srtt = self.srtt.unwrap_or(self.rto);
            let reorder = self.min_rtt.unwrap_or(srtt) / 4;
            let st = &mut self.streams[u];
            for (&s, f) in st.inflight.range_mut(..h.saturating_sub(DUP_THRESHOLD - 1)) {
                debug_assert!(s < h);
                if !f.fast && f.due > now && h >= f.epoch.saturating_add(DUP_THRESHOLD - 1) {
                    f.fast = true;
                    let due = f.due.min(now.max(f.sent_at + srtt + reorder));
                    st.by_due.remove(&(f.due, s));
                    st.by_due.insert((due, s));
                    f.due = due;
                }
            }
        }
        if newly > 0 {
            self.streams[u].probe_used = false;
            self.arm_probe(u, now);
        }
    }

    /// Seeds the estimator from a handshake round trip, so the first
    /// retransmission doesn't wait for the conservative initial RTO.
    pub(crate) fn seed_rtt(&mut self, r: Duration) {
        if self.srtt.is_none() {
            self.update_rtt(r);
        }
        self.path_min_rtt = Some(self.path_min_rtt.map_or(r, |m| m.min(r)));
    }

    /// Lowest RTT from the handshake or any sample. Not the congestion
    /// control's `min_rtt`, which leaves out the handshake: a browser peer
    /// acknowledges data later than it answers a handshake.
    pub(crate) fn path_min_rtt(&self) -> Option<Duration> {
        self.path_min_rtt
    }

    fn update_rtt(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(r);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((srtt * 7 + r) / 8);
            }
        }
        let rto = self.srtt.unwrap() + (self.rttvar * 4).max(Duration::from_millis(5)) + ACK_DELAY;
        self.rto = rto.clamp(MIN_RTO, MAX_RTO);
    }

    pub(crate) fn has_sendable(&self, now: Instant, window: usize) -> bool {
        (0..2).any(|u| self.may_send_new(u, window) || self.streams[u].has_due(now))
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        let due = |s: &Stream| [earliest(&s.by_due), s.probe_at, earliest(&s.by_dup)];
        self.streams.iter().flat_map(due).flatten().min()
    }

    /// Appends the urgent stream's frames, then the reliable stream's, while they fit in `max` bytes.
    pub(crate) fn write_frames(&mut self, now: Instant, window: usize, out: &mut Vec<u8>, max: usize) {
        for u in [URG, REL] {
            self.write_stream(u, now, window, out, max);
        }
    }

    /// Appends due retransmissions, then new fragments, of stream `u`.
    fn write_stream(&mut self, u: usize, now: Instant, window: usize, out: &mut Vec<u8>, max: usize) {
        let urgent = u == URG;
        let mut lost = false;
        let mut timed_out = false;
        let mut resent = false;
        let st = &mut self.streams[u];
        let sent_end = st.sent_end;
        for seq in due_by_seq(&st.by_due, now) {
            let f = st.inflight.get_mut(&seq).expect("indexed fragment in flight");
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            lost = true;
            timed_out |= !f.fast;
            resent = true;
            f.fast = false;
            f.epoch = sent_end;
            if let Some(d) = f.dup_at {
                st.by_dup.remove(&(d, seq));
            }
            f.dup_at = urgent.then_some(now + URGENT_GAP);
            if let Some(d) = f.dup_at {
                st.by_dup.insert((d, seq));
            }
            wire::put_reliable(out, seq, f.fin, &f.data, urgent);
            f.tries += 1;
            if f.tries > MAX_TRIES {
                self.failed = true;
            }
            f.sent_at = now;
            let backoff = self.rto.saturating_mul(1 << (f.tries - 1).min(8)).min(MAX_BACKOFF);
            st.by_due.remove(&(f.due, seq));
            f.due = now + backoff;
            st.by_due.insert((f.due, seq));
            st.retransmits += 1;
        }
        if lost {
            let random = !timed_out && self.queue_delay.is_some_and(|q| q < RANDOM_LOSS_QUEUE);
            self.back_off(now, if random { RANDOM_LOSS_CUT } else { LOSS_CUT });
        }
        let st = &mut self.streams[u];
        for seq in due_by_seq(&st.by_dup, now) {
            let f = st.inflight.get_mut(&seq).expect("indexed fragment in flight");
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            st.by_dup.remove(&(f.dup_at.expect("indexed duplicate"), seq));
            wire::put_reliable(out, seq, f.fin, &f.data, urgent);
            // Still timed from the first copy: at most URGENT_GAP off, whereas no
            // samples at all would leave an urgent-only link's RTO stale and fire
            // it on every queued message once the sender's uplink backs up.
            f.dup_at = None;
            self.urgent_copies += 1;
        }
        if earliest(&st.by_due).is_some_and(|d| d <= now) {
            return; // retransmissions first
        }
        let can_send_new = self.may_send_new(u, window);
        let st = &mut self.streams[u];
        if st.probe_at.is_some_and(|p| p <= now) {
            st.probe_at = None;
            // New data or a retransmission probes just as well.
            if !can_send_new && !resent {
                if let Some((&seq, f)) = st.inflight.iter_mut().next_back() {
                    if out.len() + REL_OVERHEAD + f.data.len() <= max {
                        wire::put_reliable(out, seq, f.fin, &f.data, urgent);
                        f.probed = true;
                        st.probe_used = true;
                        st.probes += 1;
                        st.retransmits += 1;
                    }
                }
            }
        }
        let mut sent_new = false;
        while self.may_send_new(u, window) {
            let st = &mut self.streams[u];
            let Some((_, _, data)) = st.queued.front() else { break };
            if out.len() + REL_OVERHEAD + data.len() > max {
                break;
            }
            let (seq, fin, data) = st.queued.pop_front().unwrap();
            wire::put_reliable(out, seq, fin, &data, urgent);
            let dup_at = urgent.then_some(now + URGENT_GAP);
            st.by_due.insert((now + self.rto, seq));
            st.inflight_bytes += REL_OVERHEAD + data.len();
            if let Some(d) = dup_at {
                st.by_dup.insert((d, seq));
            }
            st.inflight.insert(
                seq,
                Fragment {
                    fin,
                    data,
                    sent_at: now,
                    due: now + self.rto,
                    tries: 1,
                    fast: false,
                    epoch: seq + 1,
                    probed: false,
                    dup_at,
                },
            );
            sent_new = true;
            st.sent_end = seq + 1;
            st.fragments_sent += 1;
        }
        let st = &self.streams[u];
        if sent_new || (st.probe_at.is_none() && !st.probe_used) {
            self.arm_probe(u, now);
        }
    }
}

fn earliest(index: &BTreeSet<(Instant, u32)>) -> Option<Instant> {
    index.first().map(|&(t, _)| t)
}

/// Sequence numbers indexed at or before `now`, in sequence order (the order
/// fragments go out in), without walking the rest of the flight.
fn due_by_seq(index: &BTreeSet<(Instant, u32)>, now: Instant) -> Vec<u32> {
    if earliest(index).is_none_or(|t| t > now) {
        return Vec::new();
    }
    let mut seqs: Vec<u32> = index.range(..=(now, u32::MAX)).map(|&(_, s)| s).collect();
    seqs.sort_unstable();
    seqs
}

pub(crate) struct RelRecv {
    /// Fragments buffered at most, from `next` on: RECV_WINDOW or URGENT_RECV_WINDOW.
    window: u32,
    next: u32,
    buf: BTreeMap<u32, (bool, Vec<u8>)>,
    assembling: Vec<u8>,
    pub(crate) ack_pending: bool,
}

impl RelRecv {
    pub(crate) fn new(window: u32) -> Self {
        RelRecv { window, next: 0, buf: BTreeMap::new(), assembling: Vec::new(), ack_pending: false }
    }

    /// Accepts one fragment and appends any messages it completes to `out`.
    /// `Err` means the peer broke the protocol (oversized message).
    pub(crate) fn on_fragment(
        &mut self,
        seq: u32,
        fin: bool,
        data: &[u8],
        max_message: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), ()> {
        self.ack_pending = true;
        if seq < self.next || seq - self.next >= self.window || self.buf.contains_key(&seq) {
            return Ok(());
        }
        self.buf.insert(seq, (fin, data.to_vec()));
        while let Some((fin, d)) = self.buf.remove(&self.next) {
            self.next = self.next.checked_add(1).ok_or(())?;
            if self.assembling.len() + d.len() > max_message {
                return Err(());
            }
            self.assembling.extend_from_slice(&d);
            if fin {
                out.push(std::mem::take(&mut self.assembling));
            }
        }
        Ok(())
    }

    /// Bytes held: fragments buffered out of order and a message partly assembled.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.buf.values().map(|(_, d)| d.len()).sum::<usize>() + self.assembling.len()
    }

    pub(crate) fn ack(&self) -> (u32, u64) {
        let mut bits = 0u64;
        for &s in self.buf.range(self.next.saturating_add(1)..self.next.saturating_add(65)).map(|(s, _)| s) {
            bits |= 1 << (s - self.next - 1);
        }
        (self.next, bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{frames, Frame};

    fn drain(tx: &mut RelSend, now: Instant) -> Vec<(u32, bool, Vec<u8>)> {
        let mut out = Vec::new();
        tx.write_frames(now, 256, &mut out, 100_000);
        frames(&out)
            .map(|f| match f.unwrap() {
                Frame::Reliable { seq, fin, data, .. } => (seq, fin, data.to_vec()),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn fragments_reassemble_out_of_order() {
        let mut tx = RelSend::new();
        let msg: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        tx.push_message(&msg, 1 << 20, 1 << 20, false).ok().unwrap();
        tx.push_message(b"", 1 << 20, 1 << 20, false).ok().unwrap();
        let mut frags = drain(&mut tx, Instant::ZERO);
        assert_eq!(frags.len(), 6);
        frags.reverse();
        let mut rx = RelRecv::new(RECV_WINDOW);
        let mut out = Vec::new();
        for (s, fin, d) in &frags {
            rx.on_fragment(*s, *fin, d, 1 << 20, &mut out).unwrap();
        }
        assert_eq!(out, vec![msg, vec![]]);
    }

    #[test]
    fn ack_and_retransmit() {
        let mut tx = RelSend::new();
        for i in 0..3u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 3);
        // seq 0 lost, 1 and 2 arrived: cum=0, bits for 1 and 2
        tx.on_ack(Instant::from_millis(50), 0, 0b11, false);
        assert!(drain(&mut tx, Instant::from_millis(100)).is_empty());
        let re = drain(&mut tx, Instant::from_millis(400));
        assert_eq!(re, vec![(0, true, vec![0])]);
        assert_eq!(tx.retransmits(), 1);
        assert_eq!(tx.fragments_sent(), 3, "a retransmission is not a new fragment");
        tx.on_ack(Instant::from_millis(450), 3, 0, false);
        assert_eq!(tx.next_due(), None);
    }

    #[test]
    fn fast_retransmit_once_per_loss_event() {
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..64u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 16);
        let resends_of_0 = |frags: &[(u32, bool, Vec<u8>)]| frags.iter().filter(|f| f.0 == 0).count();
        // Seq 0 is lost; 1..16 arrive and are acknowledged one by one, past
        // the reordering window. The first three expose the gap, the rest were
        // already in flight when 0 was resent and are no news about that copy,
        // even those acknowledged more than a round trip after it.
        let mut resent = 0;
        let mut t = Instant::from_millis(130);
        for k in 1..16u32 {
            tx.on_ack(t, 0, (1u64 << k) - 1, false);
            resent += resends_of_0(&drain(&mut tx, t));
            t = t + Duration::from_millis(20);
        }
        assert_eq!(resent, 1, "one loss, one fast retransmit");
        assert!(!tx.failed);
        // That copy is lost too: fragments sent after it are acknowledged, so
        // it goes once more (at the ACK for 22, three past the 20 sent after
        // it), and only once while those later fragments are acknowledged.
        for k in 16..32u32 {
            tx.on_ack(t, 0, (1u64 << k) - 1, false);
            resent += resends_of_0(&drain(&mut tx, t));
            t = t + Duration::from_millis(10);
        }
        assert_eq!(resent, 2);
        assert!(!tx.failed);
    }

    #[test]
    fn reordered_fragment_is_not_resent() {
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..16u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 16);
        // Seq 0 is overtaken by 1..8: it is resent only if still missing a
        // quarter RTT after its ACK was due.
        let t = Instant::from_millis(100);
        tx.on_ack(t, 0, 0xff, false);
        assert!(drain(&mut tx, t).iter().all(|f| f.0 != 0));
        assert_eq!(tx.next_due(), Some(Instant::from_millis(125)));
        tx.on_ack(Instant::from_millis(110), 16, 0, false);
        assert_eq!(tx.retransmits(), 0);
        assert!(tx.cwnd() > INITIAL_CWND as u32, "no window cut: {}", tx.cwnd());
        // Still missing after the window: resent, and the window is cut then.
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..16u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        tx.on_ack(t, 0, 0xff, false);
        let cwnd = tx.cwnd();
        assert_eq!(drain(&mut tx, Instant::from_millis(125)), vec![(0, true, vec![0])]);
        assert!(tx.cwnd() < cwnd);
        // The ACKs showed no queueing, so the loss is taken as random: 80% of
        // the 8 in flight (0 and 9..15) is kept.
        assert_eq!(tx.ssthresh, 8.0 * RANDOM_LOSS_CUT);
        assert_eq!(tx.cwnd(), 6);
    }

    /// The other losses halve: one found by fast retransmit when the latest RTT
    /// sample showed RANDOM_LOSS_QUEUE of queueing, and a timeout.
    #[test]
    fn queued_or_timed_out_loss_halves_the_window() {
        let sent = || {
            let mut tx = RelSend::new();
            tx.seed_rtt(Duration::from_millis(100));
            for i in 0..16u8 {
                tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
            }
            assert_eq!(drain(&mut tx, Instant::ZERO).len(), 16);
            tx
        };
        // 1 and 2 set the minimum RTT; 3..8 come back 5 ms later; 0 is lost.
        let mut tx = sent();
        tx.on_ack(Instant::from_millis(100), 0, 0b11, false);
        tx.on_ack(Instant::from_millis(100) + RANDOM_LOSS_QUEUE, 0, 0xff, false);
        assert_eq!(tx.delay_backoffs, 0);
        assert_eq!(drain(&mut tx, Instant::from_millis(200)), vec![(0, true, vec![0])]);
        assert_eq!(tx.ssthresh, 8.0 * LOSS_CUT);
        // No queueing, but 15 time out (0 was acknowledged): halved too.
        let mut tx = sent();
        tx.on_ack(Instant::from_millis(100), 1, 0, false);
        let due = earliest(&tx.streams[REL].by_due).unwrap();
        assert_eq!(drain(&mut tx, due).len(), 15);
        assert_eq!(tx.retransmits(), 15);
        assert_eq!(tx.ssthresh, 15.0 * LOSS_CUT);
    }

    #[test]
    fn queueing_delay_keeps_85_percent_of_the_window() {
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..64u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 16);
        // Seq 0 sets the minimum RTT; seq 1 comes back 30 ms later, over QUEUE_TARGET.
        tx.on_ack(Instant::from_millis(100), 1, 0, false);
        assert_eq!(tx.delay_backoffs, 0);
        tx.on_ack(Instant::from_millis(130), 2, 0, false);
        assert_eq!(tx.delay_backoffs, 1);
        assert_eq!(tx.retransmits(), 0);
        assert_eq!(tx.ssthresh, 14.0 * DELAY_CUT);
        assert_eq!(tx.cwnd(), 11, "14 in flight: 85% kept, not halved");
        // Only one cut per round trip.
        tx.on_ack(Instant::from_millis(140), 3, 0, false);
        assert_eq!(tx.delay_backoffs, 1);
    }

    #[test]
    fn retransmissions_go_out_in_sequence_order() {
        let mut tx = RelSend::new();
        tx.push_message(b"a", 1 << 20, 1 << 20, false).ok().unwrap();
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 1);
        tx.push_message(b"b", 1 << 20, 1 << 20, false).ok().unwrap();
        // 0 times out and backs off past 1, which is sent new at the same time.
        let t = Instant::ZERO + INITIAL_RTO;
        let seqs = |f: Vec<(u32, bool, Vec<u8>)>| f.into_iter().map(|f| f.0).collect::<Vec<_>>();
        assert_eq!(seqs(drain(&mut tx, t)), vec![0, 1]);
        assert_eq!(tx.next_due(), Some(t + INITIAL_RTO));
        // Both due: sequence order, not due order.
        assert_eq!(seqs(drain(&mut tx, t + INITIAL_RTO * 2)), vec![0, 1]);
    }

    #[test]
    fn tail_probe_beats_rto() {
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(40));
        tx.push_message(b"a", 1 << 20, 1 << 20, false).ok().unwrap();
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 1);
        // The lone fragment is lost: the probe is due well before the RTO.
        let probe = tx.next_due().unwrap();
        assert!(probe < Instant::ZERO + tx.rto, "{probe:?} vs rto {:?}", tx.rto);
        assert!(drain(&mut tx, Instant::from_micros(probe.as_micros() - 1000)).is_empty());
        assert_eq!(drain(&mut tx, probe), vec![(0, true, b"a".to_vec())]);
        assert_eq!(tx.streams[REL].probes, 1);
        // Only one probe per flight; the probed copy's ACK gives no RTT sample.
        assert_eq!(tx.next_due(), Some(Instant::ZERO + tx.rto));
        let srtt = tx.srtt();
        tx.on_ack(probe + Duration::from_millis(40), 1, 0, false);
        assert_eq!(tx.srtt(), srtt);
        assert_eq!(tx.next_due(), None);
    }

    #[test]
    fn urgent_fragment_is_sent_twice() {
        let mut tx = RelSend::new();
        tx.push_message(b"u", 1 << 20, 1 << 20, true).ok().unwrap();
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 1);
        assert_eq!(tx.next_due(), Some(Instant::ZERO + URGENT_GAP));
        assert_eq!(drain(&mut tx, Instant::ZERO + URGENT_GAP), vec![(0, true, b"u".to_vec())]);
        assert_eq!(tx.urgent_copies, 1);
        // Acknowledged: nothing more goes out, and the first copy is timed.
        tx.on_ack(Instant::from_millis(30), 1, 0, true);
        assert_eq!(tx.next_due(), None);
        assert_eq!(tx.srtt(), Some(Duration::from_millis(30)));
    }

    #[test]
    fn urgent_goes_ahead_of_queued_reliable() {
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..64u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        assert_eq!(drain(&mut tx, Instant::ZERO).len(), 16, "the window is full");
        tx.push_message(b"u", 1 << 20, 1 << 20, true).ok().unwrap();
        assert!(drain(&mut tx, Instant::from_millis(1)).is_empty(), "urgent stays within the window");
        // An ACK frees two slots: the urgent fragment takes the first, in its
        // own sequence space, and goes first in the packet.
        tx.on_ack(Instant::from_millis(100), 1, 0, false);
        let mut out = Vec::new();
        tx.write_frames(Instant::from_millis(100), 256, &mut out, 100_000);
        let got: Vec<_> = frames(&out).map(|f| f.unwrap()).collect();
        assert_eq!(got[0], Frame::Reliable { seq: 0, fin: true, data: b"u", urgent: true });
        assert!(matches!(got[1], Frame::Reliable { seq: 16, urgent: false, .. }), "{got:?}");
    }

    #[test]
    fn urgent_takes_at_most_half_the_window_while_reliable_waits() {
        // 1000 B urgent fragments, sent twice, take about two slots each.
        let mut tx = RelSend::new();
        for i in 0..64u8 {
            tx.push_message(&[i; 1000], 1 << 20, 1 << 20, true).ok().unwrap();
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        assert_eq!([tx.urgent_slots(), tx.slots()], [8, 16]);
        assert_eq!([tx.streams[URG].inflight.len(), tx.streams[REL].inflight.len()], [4, 8]);
        // With nothing else to send, urgent may fill the window (15 slots used
        // before the last fragment, like a reliable one sent at 15 of 16).
        let mut tx = RelSend::new();
        for i in 0..64u8 {
            tx.push_message(&[i; 1000], 1 << 20, 1 << 20, true).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        assert_eq!(tx.streams[URG].inflight.len(), 9);
        // Small inputs barely dent the window: 64 of 8 B, sent twice, take 2 slots.
        let mut tx = RelSend::new();
        for i in 0..64u8 {
            tx.push_message(&[i; 8], 1 << 20, 1 << 20, true).ok().unwrap();
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        assert_eq!([tx.streams[URG].inflight.len(), tx.streams[REL].inflight.len()], [64, 14]);
    }

    #[test]
    fn the_window_grows_by_the_slots_acknowledged() {
        // Sixteen 8 B urgent fragments take half a slot between them, so their
        // ACKs grow the window by that much, not by sixteen fragments.
        let mut tx = RelSend::new();
        for i in 0..16u8 {
            tx.push_message(&[i; 8], 1 << 20, 1 << 20, true).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        tx.on_ack(Instant::from_millis(50), 16, 0, true);
        assert_eq!(tx.cwnd(), 16);
        // A reliable fragment is a whole slot.
        tx.push_message(b"r", 1 << 20, 1 << 20, false).ok().unwrap();
        drain(&mut tx, Instant::from_millis(50));
        tx.on_ack(Instant::from_millis(100), 1, 0, false);
        assert_eq!(tx.cwnd(), 17);
    }

    #[test]
    fn each_stream_stays_within_the_receivers_window() {
        // Small urgent fragments take a fraction of a slot each, but the
        // receiver buffers at most `max` fragments of each stream.
        let mut tx = RelSend::new();
        for i in 0..64u8 {
            tx.push_message(&[i; 8], 1 << 20, 1 << 20, true).ok().unwrap();
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        tx.write_frames(Instant::ZERO, 8, &mut Vec::new(), 100_000);
        assert_eq!([tx.streams[URG].inflight.len(), tx.streams[REL].inflight.len()], [8, 8]);
    }

    #[test]
    fn urgent_fragments_stay_within_the_urgent_window() {
        // The span from the oldest unacknowledged urgent fragment to the newest
        // stays under URGENT_RECV_WINDOW: one hole stops new urgent fragments,
        // however many after it were acknowledged.
        let mut tx = RelSend::new();
        for i in 0..200u8 {
            tx.push_message(&[i; 8], 1 << 20, 1 << 20, true).ok().unwrap();
        }
        let first = drain(&mut tx, Instant::ZERO);
        assert_eq!(first.len(), URGENT_RECV_WINDOW as usize);
        tx.on_ack(Instant::from_millis(1), 0, u64::MAX >> 1, true);
        assert_eq!(tx.streams[URG].inflight.len(), 1);
        assert!(drain(&mut tx, Instant::from_millis(1)).is_empty());
        tx.on_ack(Instant::from_millis(2), 1, 0, true);
        let next = drain(&mut tx, Instant::from_millis(2));
        assert_eq!(next.first().map(|f| f.0), Some(64));
        assert_eq!(next.len(), URGENT_RECV_WINDOW as usize);
    }

    #[test]
    fn the_urgent_receiver_buffers_at_most_its_window() {
        let mut rx = RelRecv::new(URGENT_RECV_WINDOW);
        let mut out = Vec::new();
        for seq in 1..200 {
            rx.on_fragment(seq, true, &[1; 100], MAX_URGENT, &mut out).unwrap();
        }
        assert_eq!(rx.buf.len(), URGENT_RECV_WINDOW as usize - 1);
        rx.on_fragment(0, true, &[0; 100], MAX_URGENT, &mut out).unwrap();
        assert_eq!(out.len(), URGENT_RECV_WINDOW as usize);
        // An urgent message over MAX_URGENT breaks the protocol.
        let mut rx = RelRecv::new(URGENT_RECV_WINDOW);
        for seq in 0..(MAX_URGENT / FRAGMENT) as u32 {
            rx.on_fragment(seq, false, &[0; FRAGMENT], MAX_URGENT, &mut out).unwrap();
        }
        assert!(rx.on_fragment(59, true, &[0; FRAGMENT], MAX_URGENT, &mut out).is_err());
    }

    #[test]
    fn each_stream_has_its_own_buffer() {
        let mut tx = RelSend::new();
        assert!(tx.push_message(&[0; 600], 1000, 1 << 20, false).is_ok());
        assert!(tx.push_message(&[0; 600], 1000, 1 << 20, false).is_err());
        assert!(tx.push_message(&[0; 600], 1000, 1 << 20, true).is_ok());
    }

    /// The indexes match the fragments, and the lookups agree with the full
    /// scans they replace.
    fn check(tx: &RelSend, now: Instant, window: usize) {
        let w = tx.window();
        let wire = |st: &Stream| st.inflight.values().map(|f| REL_OVERHEAD + f.data.len()).sum::<usize>();
        let urgent_slots = (2 * wire(&tx.streams[URG])).div_ceil(REL_OVERHEAD + FRAGMENT);
        let slots = tx.streams[REL].inflight.len() + urgent_slots;
        let mut sendable = false;
        for (u, st) in tx.streams.iter().enumerate() {
            assert_eq!(st.inflight_bytes, wire(st));
            let due: BTreeSet<_> = st.inflight.iter().map(|(&s, f)| (f.due, s)).collect();
            let dup: BTreeSet<_> = st.inflight.iter().filter_map(|(&s, f)| f.dup_at.map(|d| (d, s))).collect();
            assert_eq!(st.by_due, due);
            assert_eq!(st.by_dup, dup);
            let urgent_room = tx.streams[REL].queued.is_empty() || urgent_slots < w.div_ceil(2);
            let span = st.inflight.first_key_value().map_or(0, |(&lo, _)| st.sent_end - lo);
            let room =
                st.inflight.len() < window && slots < w && (u == REL || urgent_room && span < URGENT_RECV_WINDOW);
            sendable |= (!st.queued.is_empty() && room)
                || st.inflight.values().any(|f| f.due <= now)
                || st.probe_at.is_some_and(|p| p <= now)
                || st.inflight.values().any(|f| f.dup_at.is_some_and(|d| d <= now));
        }
        assert_eq!(tx.has_sendable(now, window), sendable);
        let next = tx
            .streams
            .iter()
            .flat_map(|st| st.inflight.values().flat_map(|f| [Some(f.due), f.dup_at]).chain([st.probe_at]))
            .flatten()
            .min();
        assert_eq!(tx.next_due(), next);
    }

    #[test]
    fn due_indexes_match_a_full_scan() {
        for seed in 1..=24u64 {
            let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let mut rand = move |n: u64| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % n
            };
            let (mut tx, mut rx) = (RelSend::new(), [RelRecv::new(RECV_WINDOW), RelRecv::new(URGENT_RECV_WINDOW)]);
            let window = 64;
            // One-way delay: a LAN (a retransmission can go out while its
            // urgent copy is pending) or a WAN.
            let base = if seed % 2 == 0 { 20_000 } else { 400 };
            let mut now = Instant::ZERO;
            // (arrival, stream, frame) on a lossy link that reorders, and ACKs back.
            let mut to_rx: Vec<(Instant, usize, u32, bool, Vec<u8>)> = Vec::new();
            let mut to_tx: Vec<(Instant, usize, u32, u64)> = Vec::new();
            let mut delivered = Vec::new();
            for _ in 0..1500 {
                now = now + Duration::from_micros(rand(base * 2 / 5));
                if rand(4) == 0 {
                    let len = rand(4000) as usize;
                    let _ = tx.push_message(&vec![7; len], 1 << 16, 1 << 20, rand(5) == 0);
                    check(&tx, now, window);
                }
                // Retransmissions go first in each stream, in sequence order, as the scan sent them.
                let due: Vec<Vec<u32>> = (0..2)
                    .map(|u| tx.streams[u].inflight.iter().filter(|(_, f)| f.due <= now).map(|(&s, _)| s).collect())
                    .collect();
                let before: Vec<u64> = tx.streams.iter().map(|s| s.retransmits - s.probes).collect();
                let mut out = Vec::new();
                tx.write_frames(now, window, &mut out, 1200 + rand(3000) as usize);
                check(&tx, now, window);
                for u in 0..2 {
                    let resent = (tx.streams[u].retransmits - tx.streams[u].probes - before[u]) as usize;
                    let sent: Vec<u32> = frames(&out)
                        .filter_map(|f| match f.unwrap() {
                            Frame::Reliable { seq, urgent, .. } => (urgent == (u == URG)).then_some(seq),
                            _ => unreachable!(),
                        })
                        .take(resent)
                        .collect();
                    assert_eq!(sent, due[u][..resent]);
                }
                for f in frames(&out) {
                    let Frame::Reliable { seq, fin, data, urgent } = f.unwrap() else { unreachable!() };
                    if rand(10) != 0 {
                        let at = now + Duration::from_micros(base + rand(base * 3 / 4));
                        to_rx.push((at, urgent as usize, seq, fin, data.to_vec()));
                    }
                }
                to_rx.retain(|(at, u, seq, fin, data)| {
                    let keep = *at > now;
                    if !keep {
                        rx[*u].on_fragment(*seq, *fin, data, 1 << 20, &mut delivered).unwrap();
                    }
                    keep
                });
                for (u, rx) in rx.iter_mut().enumerate() {
                    if rx.ack_pending && rand(3) != 0 {
                        rx.ack_pending = false;
                        let (cum, bits) = rx.ack();
                        to_tx.push((now + Duration::from_micros(base + rand(base / 2)), u, cum, bits));
                    }
                }
                let mut i = 0;
                while i < to_tx.len() {
                    if to_tx[i].0 <= now {
                        let (_, u, cum, bits) = to_tx.swap_remove(i);
                        tx.on_ack(now, cum, bits, u == URG);
                        check(&tx, now, window);
                    } else {
                        i += 1;
                    }
                }
            }
            // On the LAN an urgent fragment is mostly acknowledged before its copy.
            assert!(tx.retransmits() > 0 && !delivered.is_empty(), "seed {seed}");
            assert!(tx.urgent_copies > 0 || base < 1000, "seed {seed}");
        }
    }

    #[test]
    fn buffer_limit() {
        let mut tx = RelSend::new();
        assert!(tx.push_message(&[0; 600], 1000, 1 << 20, false).is_ok());
        assert!(matches!(tx.push_message(&[0; 600], 1000, 1 << 20, false), Err(PushError::BufferFull)));
        assert!(matches!(tx.push_message(&[0; 600], 1 << 20, 500, false), Err(PushError::TooLarge)));
    }
}
