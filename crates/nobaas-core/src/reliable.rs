//! Reliable-ordered lane: fragmentation, selective ACK, RTO retransmission, reassembly.

use crate::time::Instant;
use crate::wire::{self, FRAGMENT, REL_OVERHEAD};
use std::collections::{BTreeMap, VecDeque};
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
/// presumed lost and resent without waiting for its timeout.
const DUP_THRESHOLD: u32 = 3;
/// Queueing delay (RTT above the minimum seen) at which the window backs off,
/// before any loss: keeps buffers short on paths that never drop (a browser
/// DataChannel, a bloated router), which real-time traffic needs.
const QUEUE_TARGET: Duration = Duration::from_millis(25);
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
    /// Sent again as a tail probe: its ACK can't be timed (Karn's rule).
    probed: bool,
    /// Every transmission (first and retransmissions) is sent twice.
    urgent: bool,
    /// When to send the duplicate copy of the latest transmission.
    dup_at: Option<Instant>,
}

pub(crate) struct RelSend {
    next_seq: u32,
    /// (seq, fin, urgent, data)
    queued: VecDeque<(u32, bool, bool, Vec<u8>)>,
    inflight: BTreeMap<u32, Fragment>,
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
            queued: VecDeque::new(),
            inflight: BTreeMap::new(),
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

    /// Halves the window at most once per round trip.
    fn on_loss(&mut self, now: Instant) {
        if now < self.recover_until {
            return;
        }
        self.ssthresh = (self.inflight.len() as f64 / 2.0).max(MIN_CWND);
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
        }
        if queued_too_long {
            if now >= self.recover_until {
                self.delay_backoffs += 1;
                self.on_loss(now);
            }
        } else if newly > 0 {
            if self.cwnd < self.ssthresh {
                self.cwnd += newly as f64;
            } else {
                self.cwnd += newly as f64 / self.cwnd;
            }
            self.cwnd = self.cwnd.min(65536.0);
        }
        // Fast retransmit: fragments well below the highest acknowledged one.
        if let Some(h) = highest {
            let mut lost = false;
            for (&s, f) in self.inflight.range_mut(..h.saturating_sub(DUP_THRESHOLD - 1)) {
                debug_assert!(s < h);
                if !f.fast && f.due > now {
                    f.fast = true;
                    f.due = now;
                    lost = true;
                }
            }
            if lost {
                self.on_loss(now);
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
            || self.inflight.values().any(|f| f.due <= now)
            || self.probe_at.is_some_and(|p| p <= now)
            || self.inflight.values().any(|f| f.dup_at.is_some_and(|d| d <= now))
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        let dups = self.inflight.values().filter_map(|f| f.dup_at);
        self.inflight.values().map(|f| f.due).chain(self.probe_at).chain(dups).min()
    }

    /// Appends due retransmissions, then new fragments, while they fit in `max` bytes.
    pub(crate) fn write_frames(&mut self, now: Instant, window: usize, out: &mut Vec<u8>, max: usize) {
        let mut timed_out = false;
        let mut resent = false;
        for (&seq, f) in self.inflight.iter_mut() {
            if f.due > now {
                continue;
            }
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            timed_out |= !f.fast;
            resent = true;
            f.fast = false;
            f.dup_at = f.urgent.then_some(now + URGENT_GAP);
            wire::put_reliable(out, seq, f.fin, &f.data);
            f.tries += 1;
            if f.tries > MAX_TRIES {
                self.failed = true;
            }
            f.sent_at = now;
            let backoff = self.rto.saturating_mul(1 << (f.tries - 1).min(8)).min(MAX_BACKOFF);
            f.due = now + backoff;
            self.retransmits += 1;
        }
        if timed_out {
            self.on_loss(now);
        }
        for (&seq, f) in self.inflight.iter_mut() {
            if !f.dup_at.is_some_and(|d| d <= now) {
                continue;
            }
            if out.len() + REL_OVERHEAD + f.data.len() > max {
                break;
            }
            wire::put_reliable(out, seq, f.fin, &f.data);
            // Still timed from the first copy: at most URGENT_GAP off, whereas no
            // samples at all would leave an urgent-only link's RTO stale and fire
            // it on every queued message once the sender's uplink backs up.
            f.dup_at = None;
            self.urgent_copies += 1;
        }
        if self.inflight.values().any(|f| f.due <= now) {
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
            self.inflight.insert(
                seq,
                Fragment {
                    fin,
                    data,
                    sent_at: now,
                    due: now + self.rto,
                    tries: 1,
                    fast: false,
                    probed: false,
                    urgent,
                    dup_at,
                },
            );
            sent_new = true;
            self.fragments_sent += 1;
        }
        if sent_new || (self.probe_at.is_none() && !self.probe_used) {
            self.arm_probe(now);
        }
    }
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

    #[test]
    fn buffer_limit() {
        let mut tx = RelSend::new();
        assert!(tx.push_message(&[0; 600], 1000, 1 << 20, false).is_ok());
        assert!(matches!(tx.push_message(&[0; 600], 1000, 1 << 20, false), Err(PushError::BufferFull)));
        assert!(matches!(tx.push_message(&[0; 600], 1 << 20, 500, false), Err(PushError::TooLarge)));
    }
}
