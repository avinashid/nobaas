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
    /// Every transmission (first and retransmissions) is sent twice.
    urgent: bool,
    /// When to send the duplicate copy of the latest transmission.
    dup_at: Option<Instant>,
}

pub(crate) struct RelSend {
    next_seq: u32,
    /// One past the newest fragment sent so far.
    sent_end: u32,
    /// (seq, fin, urgent, data)
    queued: VecDeque<(u32, bool, bool, Vec<u8>)>,
    inflight: BTreeMap<u32, Fragment>,
    /// `(due, seq)` and `(dup_at, seq)` of every in-flight fragment, so the
    /// earliest is found without walking `inflight` on each packet.
    by_due: BTreeSet<(Instant, u32)>,
    by_dup: BTreeSet<(Instant, u32)>,
    buffered: usize,
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    pub(crate) retransmits: u64,
    /// Fragments sent for the first time.
    pub(crate) fragments_sent: u64,
    pub(crate) failed: bool,
    /// AIMD congestion window in fragments (NewReno-style), capped by the link window.
    cwnd: f64,
    ssthresh: f64,
    /// No second reduction until this time (one per round trip).
    recover_until: Instant,
    min_rtt: Option<Duration>,
    path_min_rtt: Option<Duration>,
    /// Queueing delay of the latest RTT sample (sample − min_rtt); none before the first.
    queue_delay: Option<Duration>,
    pub(crate) delay_backoffs: u64,
    /// When to send a tail-loss probe; one per flight, re-armed on ACK progress.
    probe_at: Option<Instant>,
    probe_used: bool,
    pub(crate) probes: u64,
    pub(crate) urgent_copies: u64,
}

pub(crate) enum PushError {
    BufferFull,
    TooLarge,
}

impl RelSend {
    pub(crate) fn new() -> Self {
        RelSend {
            next_seq: 0,
            sent_end: 0,
            queued: VecDeque::new(),
            inflight: BTreeMap::new(),
            by_due: BTreeSet::new(),
            by_dup: BTreeSet::new(),
            buffered: 0,
            srtt: None,
            rttvar: Duration::ZERO,
            rto: INITIAL_RTO,
            retransmits: 0,
            fragments_sent: 0,
            failed: false,
            cwnd: INITIAL_CWND,
            ssthresh: f64::INFINITY,
            recover_until: Instant::ZERO,
            min_rtt: None,
            path_min_rtt: None,
            queue_delay: None,
            delay_backoffs: 0,
            probe_at: None,
            probe_used: false,
            probes: 0,
            urgent_copies: 0,
        }
    }

    fn window(&self, max: usize) -> usize {
        (self.cwnd as usize).clamp(MIN_CWND as usize, max.max(1))
    }

    /// Cuts the window to `keep` of the fragments in flight, at most once per round trip.
    fn back_off(&mut self, now: Instant, keep: f64) {
        if now < self.recover_until {
            return;
        }
        self.ssthresh = (self.inflight.len() as f64 * keep).max(MIN_CWND);
        self.cwnd = self.ssthresh;
        self.recover_until = now + self.srtt.unwrap_or(self.rto);
    }

    fn arm_probe(&mut self, now: Instant) {
        self.probe_at = match self.srtt {
            Some(srtt) if !self.probe_used && !self.inflight.is_empty() => {
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

    pub(crate) fn inflight(&self) -> u32 {
        self.inflight.len() as u32
    }

    pub(crate) fn push_message(
        &mut self,
        data: &[u8],
        max_buffer: usize,
        max_message: usize,
        urgent: bool,
    ) -> Result<(), PushError> {
        if data.len() > max_message {
            return Err(PushError::TooLarge);
        }
        if self.buffered + data.len() > max_buffer {
            return Err(PushError::BufferFull);
        }
        let count = data.len().div_ceil(FRAGMENT).max(1);
        if self.next_seq.checked_add(count as u32).is_none() {
            return Err(PushError::BufferFull);
        }
        for i in 0..count {
            let chunk = &data[(i * FRAGMENT).min(data.len())..((i + 1) * FRAGMENT).min(data.len())];
            self.queued.push_back((self.next_seq, i + 1 == count, urgent, chunk.to_vec()));
            self.next_seq += 1;
        }
        self.buffered += data.len();
        Ok(())
    }

    pub(crate) fn on_ack(&mut self, now: Instant, cum: u32, bits: u64) {
        let mut sample = None;
        let mut acked: Vec<u32> = self.inflight.range(..cum).map(|(&s, _)| s).collect();
        for i in 0..64u32 {
            if bits >> i & 1 == 1 {
                if let Some(s) = cum.checked_add(1 + i) {
                    acked.push(s);
                }
            }
        }
        let mut newly = 0usize;
        let mut highest = None;
        for s in acked {
            if let Some(f) = self.inflight.remove(&s) {
                self.by_due.remove(&(f.due, s));
                if let Some(d) = f.dup_at {
                    self.by_dup.remove(&(d, s));
                }
                self.buffered -= f.data.len();
                newly += 1;
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
                self.cwnd += newly as f64;
            } else {
                self.cwnd += newly as f64 / self.cwnd;
            }
            self.cwnd = self.cwnd.min(65536.0);
        }
        // Fast retransmit: fragments well below the highest acknowledged one,
        // after the reordering window. The window is cut when they are resent.
        if let Some(h) = highest {
            let srtt = self.srtt.unwrap_or(self.rto);
            let reorder = self.min_rtt.unwrap_or(srtt) / 4;
            for (&s, f) in self.inflight.range_mut(..h.saturating_sub(DUP_THRESHOLD - 1)) {
                debug_assert!(s < h);
                if !f.fast && f.due > now && h >= f.epoch.saturating_add(DUP_THRESHOLD - 1) {
                    f.fast = true;
                    let due = f.due.min(now.max(f.sent_at + srtt + reorder));
                    self.by_due.remove(&(f.due, s));
                    self.by_due.insert((due, s));
                    f.due = due;
                }
            }
        }
        if newly > 0 {
            self.probe_used = false;
            self.arm_probe(now);
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
        (!self.queued.is_empty() && self.inflight.len() < self.window(window))
            || earliest(&self.by_due).is_some_and(|d| d <= now)
            || self.probe_at.is_some_and(|p| p <= now)
            || earliest(&self.by_dup).is_some_and(|d| d <= now)
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        [earliest(&self.by_due), self.probe_at, earliest(&self.by_dup)].into_iter().flatten().min()
    }

    /// Appends due retransmissions, then new fragments, while they fit in `max` bytes.
    pub(crate) fn write_frames(&mut self, now: Instant, window: usize, out: &mut Vec<u8>, max: usize) {
        let mut lost = false;
        let mut timed_out = false;
        let mut resent = false;
        let sent_end = self.sent_end;
        for seq in due_by_seq(&self.by_due, now) {
            let f = self.inflight.get_mut(&seq).expect("indexed fragment in flight");
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            lost = true;
            timed_out |= !f.fast;
            resent = true;
            f.fast = false;
            f.epoch = sent_end;
            if let Some(d) = f.dup_at {
                self.by_dup.remove(&(d, seq));
            }
            f.dup_at = f.urgent.then_some(now + URGENT_GAP);
            if let Some(d) = f.dup_at {
                self.by_dup.insert((d, seq));
            }
            wire::put_reliable(out, seq, f.fin, &f.data);
            f.tries += 1;
            if f.tries > MAX_TRIES {
                self.failed = true;
            }
            f.sent_at = now;
            let backoff = self.rto.saturating_mul(1 << (f.tries - 1).min(8)).min(MAX_BACKOFF);
            self.by_due.remove(&(f.due, seq));
            f.due = now + backoff;
            self.by_due.insert((f.due, seq));
            self.retransmits += 1;
        }
        if lost {
            let random = !timed_out && self.queue_delay.is_some_and(|q| q < RANDOM_LOSS_QUEUE);
            self.back_off(now, if random { RANDOM_LOSS_CUT } else { LOSS_CUT });
        }
        for seq in due_by_seq(&self.by_dup, now) {
            let f = self.inflight.get_mut(&seq).expect("indexed fragment in flight");
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            self.by_dup.remove(&(f.dup_at.expect("indexed duplicate"), seq));
            wire::put_reliable(out, seq, f.fin, &f.data);
            // Still timed from the first copy: at most URGENT_GAP off, whereas no
            // samples at all would leave an urgent-only link's RTO stale and fire
            // it on every queued message once the sender's uplink backs up.
            f.dup_at = None;
            self.urgent_copies += 1;
        }
        if earliest(&self.by_due).is_some_and(|d| d <= now) {
            return; // retransmissions first
        }
        let window = self.window(window);
        let can_send_new = !self.queued.is_empty() && self.inflight.len() < window;
        if self.probe_at.is_some_and(|p| p <= now) {
            self.probe_at = None;
            // New data or a retransmission probes just as well.
            if !can_send_new && !resent {
                if let Some((&seq, f)) = self.inflight.iter_mut().next_back() {
                    if out.len() + REL_OVERHEAD + f.data.len() <= max {
                        wire::put_reliable(out, seq, f.fin, &f.data);
                        f.probed = true;
                        self.probe_used = true;
                        self.probes += 1;
                        self.retransmits += 1;
                    }
                }
            }
        }
        let mut sent_new = false;
        while self.inflight.len() < window {
            let Some((_, _, _, data)) = self.queued.front() else { break };
            if out.len() + REL_OVERHEAD + data.len() > max {
                break;
            }
            let (seq, fin, urgent, data) = self.queued.pop_front().unwrap();
            wire::put_reliable(out, seq, fin, &data);
            let dup_at = urgent.then_some(now + URGENT_GAP);
            self.by_due.insert((now + self.rto, seq));
            if let Some(d) = dup_at {
                self.by_dup.insert((d, seq));
            }
            self.inflight.insert(
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
                    urgent,
                    dup_at,
                },
            );
            sent_new = true;
            self.sent_end = seq + 1;
            self.fragments_sent += 1;
        }
        if sent_new || (self.probe_at.is_none() && !self.probe_used) {
            self.arm_probe(now);
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
    next: u32,
    buf: BTreeMap<u32, (bool, Vec<u8>)>,
    assembling: Vec<u8>,
    pub(crate) ack_pending: bool,
}

impl RelRecv {
    pub(crate) fn new() -> Self {
        RelRecv { next: 0, buf: BTreeMap::new(), assembling: Vec::new(), ack_pending: false }
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
        if seq < self.next || seq - self.next >= RECV_WINDOW || self.buf.contains_key(&seq) {
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
                Frame::Reliable { seq, fin, data } => (seq, fin, data.to_vec()),
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
        let mut rx = RelRecv::new();
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
        tx.on_ack(Instant::from_millis(50), 0, 0b11);
        assert!(drain(&mut tx, Instant::from_millis(100)).is_empty());
        let re = drain(&mut tx, Instant::from_millis(400));
        assert_eq!(re, vec![(0, true, vec![0])]);
        assert_eq!(tx.retransmits, 1);
        assert_eq!(tx.fragments_sent, 3, "a retransmission is not a new fragment");
        tx.on_ack(Instant::from_millis(450), 3, 0);
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
            tx.on_ack(t, 0, (1u64 << k) - 1);
            resent += resends_of_0(&drain(&mut tx, t));
            t = t + Duration::from_millis(20);
        }
        assert_eq!(resent, 1, "one loss, one fast retransmit");
        assert!(!tx.failed);
        // That copy is lost too: fragments sent after it are acknowledged, so
        // it goes once more (at the ACK for 22, three past the 20 sent after
        // it), and only once while those later fragments are acknowledged.
        for k in 16..32u32 {
            tx.on_ack(t, 0, (1u64 << k) - 1);
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
        tx.on_ack(t, 0, 0xff);
        assert!(drain(&mut tx, t).iter().all(|f| f.0 != 0));
        assert_eq!(tx.next_due(), Some(Instant::from_millis(125)));
        tx.on_ack(Instant::from_millis(110), 16, 0);
        assert_eq!(tx.retransmits, 0);
        assert!(tx.cwnd() > INITIAL_CWND as u32, "no window cut: {}", tx.cwnd());
        // Still missing after the window: resent, and the window is cut then.
        let mut tx = RelSend::new();
        tx.seed_rtt(Duration::from_millis(100));
        for i in 0..16u8 {
            tx.push_message(&[i], 1 << 20, 1 << 20, false).ok().unwrap();
        }
        drain(&mut tx, Instant::ZERO);
        tx.on_ack(t, 0, 0xff);
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
        tx.on_ack(Instant::from_millis(100), 0, 0b11);
        tx.on_ack(Instant::from_millis(100) + RANDOM_LOSS_QUEUE, 0, 0xff);
        assert_eq!(tx.delay_backoffs, 0);
        assert_eq!(drain(&mut tx, Instant::from_millis(200)), vec![(0, true, vec![0])]);
        assert_eq!(tx.ssthresh, 8.0 * LOSS_CUT);
        // No queueing, but 15 time out (0 was acknowledged): halved too.
        let mut tx = sent();
        tx.on_ack(Instant::from_millis(100), 1, 0);
        let due = earliest(&tx.by_due).unwrap();
        assert_eq!(drain(&mut tx, due).len(), 15);
        assert_eq!(tx.retransmits, 15);
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
        tx.on_ack(Instant::from_millis(100), 1, 0);
        assert_eq!(tx.delay_backoffs, 0);
        tx.on_ack(Instant::from_millis(130), 2, 0);
        assert_eq!(tx.delay_backoffs, 1);
        assert_eq!(tx.retransmits, 0);
        assert_eq!(tx.ssthresh, 14.0 * DELAY_CUT);
        assert_eq!(tx.cwnd(), 11, "14 in flight: 85% kept, not halved");
        // Only one cut per round trip.
        tx.on_ack(Instant::from_millis(140), 3, 0);
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
        assert_eq!(tx.probes, 1);
        // Only one probe per flight; the probed copy's ACK gives no RTT sample.
        assert_eq!(tx.next_due(), Some(Instant::ZERO + tx.rto));
        let srtt = tx.srtt();
        tx.on_ack(probe + Duration::from_millis(40), 1, 0);
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
        tx.on_ack(Instant::from_millis(30), 1, 0);
        assert_eq!(tx.next_due(), None);
        assert_eq!(tx.srtt(), Some(Duration::from_millis(30)));
    }

    /// The indexes match the fragments, and the lookups agree with the full
    /// scans they replace.
    fn check(tx: &RelSend, now: Instant, window: usize) {
        let due: BTreeSet<_> = tx.inflight.iter().map(|(&s, f)| (f.due, s)).collect();
        let dup: BTreeSet<_> = tx.inflight.iter().filter_map(|(&s, f)| f.dup_at.map(|d| (d, s))).collect();
        assert_eq!(tx.by_due, due);
        assert_eq!(tx.by_dup, dup);
        let sendable = (!tx.queued.is_empty() && tx.inflight.len() < tx.window(window))
            || tx.inflight.values().any(|f| f.due <= now)
            || tx.probe_at.is_some_and(|p| p <= now)
            || tx.inflight.values().any(|f| f.dup_at.is_some_and(|d| d <= now));
        assert_eq!(tx.has_sendable(now, window), sendable);
        let dups = tx.inflight.values().filter_map(|f| f.dup_at);
        let next = tx.inflight.values().map(|f| f.due).chain(tx.probe_at).chain(dups).min();
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
            let (mut tx, mut rx) = (RelSend::new(), RelRecv::new());
            let window = 64;
            // One-way delay: a LAN (a retransmission can go out while its
            // urgent copy is pending) or a WAN.
            let base = if seed % 2 == 0 { 20_000 } else { 400 };
            let mut now = Instant::ZERO;
            // (arrival, frame) on a lossy link that reorders, and ACKs back.
            let mut to_rx: Vec<(Instant, u32, bool, Vec<u8>)> = Vec::new();
            let mut to_tx: Vec<(Instant, u32, u64)> = Vec::new();
            let mut delivered = Vec::new();
            for _ in 0..1500 {
                now = now + Duration::from_micros(rand(base * 2 / 5));
                if rand(4) == 0 {
                    let len = rand(4000) as usize;
                    let _ = tx.push_message(&vec![7; len], 1 << 16, 1 << 20, rand(5) == 0);
                    check(&tx, now, window);
                }
                // Retransmissions go first, in sequence order, as the scan sent them.
                let due: Vec<u32> = tx.inflight.iter().filter(|(_, f)| f.due <= now).map(|(&s, _)| s).collect();
                let (retx, probes) = (tx.retransmits, tx.probes);
                let mut out = Vec::new();
                tx.write_frames(now, window, &mut out, 1200 + rand(3000) as usize);
                check(&tx, now, window);
                let resent = (tx.retransmits - retx - (tx.probes - probes)) as usize;
                let sent: Vec<u32> = frames(&out)
                    .take(resent)
                    .map(|f| match f.unwrap() {
                        Frame::Reliable { seq, .. } => seq,
                        _ => unreachable!(),
                    })
                    .collect();
                assert_eq!(sent, due[..resent]);
                for f in frames(&out) {
                    let Frame::Reliable { seq, fin, data } = f.unwrap() else { unreachable!() };
                    if rand(10) != 0 {
                        let at = now + Duration::from_micros(base + rand(base * 3 / 4));
                        to_rx.push((at, seq, fin, data.to_vec()));
                    }
                }
                to_rx.retain(|(at, seq, fin, data)| {
                    let keep = *at > now;
                    if !keep {
                        rx.on_fragment(*seq, *fin, data, 1 << 20, &mut delivered).unwrap();
                    }
                    keep
                });
                if rx.ack_pending && rand(3) != 0 {
                    rx.ack_pending = false;
                    let (cum, bits) = rx.ack();
                    to_tx.push((now + Duration::from_micros(base + rand(base / 2)), cum, bits));
                }
                let mut i = 0;
                while i < to_tx.len() {
                    if to_tx[i].0 <= now {
                        let (_, cum, bits) = to_tx.swap_remove(i);
                        tx.on_ack(now, cum, bits);
                        check(&tx, now, window);
                    } else {
                        i += 1;
                    }
                }
            }
            // On the LAN an urgent fragment is mostly acknowledged before its copy.
            assert!(tx.retransmits > 0 && !delivered.is_empty(), "seed {seed}");
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
