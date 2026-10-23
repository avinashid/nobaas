// Browser birthday punch (NB-048): the browser counterpart of the native birthday
// punch (NB-044, docs/NAT-TRAVERSAL.md). When two peers can't punch directly — one
// behind a symmetric NAT that gives a random public port per destination, the other
// behind a cone that only lets in ports it has sent to — a single socket each can't
// meet. A browser's only sockets are inside RTCPeerConnections, so the symmetric
// ("hard") side opens many connections toward the cone ("easy") side's one address,
// and the easy side's one connection adds the hard side's IP at many guessed ports.
// Because the hard connections share one certificate and one set of ICE credentials
// (the spike in test/birthday_spike.mjs confirms Chrome keeps munged credentials),
// the easy side answers them all with one answer, and whichever of the hard sockets
// a guessed check lands on completes that connection; the hard side keeps it and
// closes the rest.
//
// This file holds the deterministic, side-effect-free primitives (credentials, the
// spray port order, guessed candidates, SDP munging). The live fleet that uses them
// is wired into WebRtcTransport separately.

// How many connections the hard side opens, and how many ports the easy side sprays.
// Matched to the native defaults (BIRTHDAY_SOCKETS, BIRTHDAY_PROBES / 2): 256 mappings
// among the ~64512 usable ports, ~1024 guesses, so a guess overlaps a mapping about
// 1024 * 256 / 64512 ≈ 4 times; at ICE's ~20 checks/s a hit comes in ~10-15 s.
export const FLEET_SIZE = 256;
export const SPRAY_PORTS = 1024;
// Ports are sprayed from here up (NATs hand public ports out above 1024).
const PORT_LOW = 1024;
const PORT_RANGE = 65536 - PORT_LOW; // 64512 = 2^10 * 3^2 * 7
const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;

// Whether `ip` is a public IPv4 address: not private (RFC 1918), loopback, CGNAT
// (100.64/10), link-local, broadcast, "this network" or multicast. So a sprayed or
// fleet-targeted address can't be aimed at the victim's own LAN, router or loopback.
export function isPublicV4(ip) {
  if (!IPV4.test(ip ?? '')) return false;
  const [a, b] = ip.split('.').map(Number);
  if (a === 10 || a === 127 || a === 0 || a >= 224) return false; // 10/8, loopback, 0/8, multicast+reserved
  if (a === 172 && b >= 16 && b <= 31) return false; // 172.16/12
  if (a === 192 && b === 168) return false; // 192.168/16
  if (a === 169 && b === 254) return false; // link-local
  if (a === 100 && b >= 64 && b <= 127) return false; // CGNAT 100.64/10
  return true;
}

// One remote candidate for the easy side's `ip:port`, if public IPv4; else null. The
// fleet adds exactly this, instead of trusting the candidate list in the peer's offer.
function easyCandidate(addr) {
  const m = /^(\d+\.\d+\.\d+\.\d+):(\d+)$/.exec(addr ?? '');
  if (!m || !isPublicV4(m[1]) || +m[2] < 1 || +m[2] > 65535) return null;
  return { candidate: `candidate:pwe0 1 udp 1 ${m[1]} ${m[2]} typ srflx raddr 0.0.0.0 rport 0`, sdpMid: '0', sdpMLineIndex: 0 };
}

// A random value in [0, 1). Injected in tests; real code uses crypto.
function rand() {
  const a = new Uint32Array(1);
  (globalThis.crypto ?? { getRandomValues: (b) => b.fill(Math.floor(Math.random() * 2 ** 32)) }).getRandomValues(a);
  return a[0] / 2 ** 32;
}

// ICE ufrag and pwd (RFC 8839 lengths: ufrag ≥ 4, pwd ≥ 22), from the allowed
// alphabet, so the same pair can be set on every connection in the fleet.
export function randomCredentials(rnd = rand) {
  const CH = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const pick = (n) => Array.from({ length: n }, () => CH[Math.floor(rnd() * CH.length)]).join('');
  return { ufrag: `pw${pick(6)}`, pwd: pick(24) };
}

// `sdp` with its ICE ufrag and pwd replaced by `creds`, so every connection in the
// fleet presents the same ones (the DTLS fingerprint is already shared by the
// certificate). Only the two credential lines change; everything else is untouched.
export function shareCredentials(sdp, creds) {
  if (typeof sdp !== 'string' || !/^[A-Za-z0-9+/]{4,256}$/.test(creds?.ufrag ?? '') || !/^[A-Za-z0-9+/]{22,256}$/.test(creds?.pwd ?? '')) return sdp;
  return sdp.replace(/^a=ice-ufrag:.*$/m, `a=ice-ufrag:${creds.ufrag}`).replace(/^a=ice-pwd:.*$/m, `a=ice-pwd:${creds.pwd}`);
}

// `count` distinct ports in [PORT_LOW, 65535], each visited once, in a pseudo-random
// order: PORT_LOW + (start + k·stride) mod PORT_RANGE with stride coprime to the
// range (odd, not a multiple of 3 or 7). The same order as the native spray.
export function sprayPorts(count, rnd = rand) {
  const start = Math.floor(rnd() * PORT_RANGE);
  let stride = (Math.floor(rnd() * PORT_RANGE) | 1) % PORT_RANGE || 1;
  while (stride % 3 === 0 || stride % 7 === 0) stride = (stride + 2) % PORT_RANGE || 1;
  const n = Math.min(count, PORT_RANGE);
  return Array.from({ length: n }, (_, k) => PORT_LOW + (start + k * stride) % PORT_RANGE);
}

// Remote ICE candidates for the easy side to add: the hard side's public IPv4 `ip`
// at each of `ports`, as low-priority host candidates with distinct foundations (so
// each is its own pair and gets its own connectivity check). `[]` unless `ip` is a
// plain IPv4 address, so nothing can be spliced into a candidate line.
export function sprayCandidates(ip, ports, sdpMid = '0') {
  if (!IPV4.test(ip ?? '')) return [];
  return ports.filter((p) => p >= 1 && p <= 65535).map((p, i) => ({
    candidate: `candidate:pwb${i} 1 udp 1 ${ip} ${p} typ host`,
    sdpMid,
    sdpMLineIndex: 0,
  }));
}

// A negotiated, unreliable datagram channel (id 0), the same on both sides, so no
// in-band negotiation is needed and either side's channel opening means the link is up.
function dataChannel(pc) {
  const dc = pc.createDataChannel('peerwire', { negotiated: true, id: 0, ordered: false, maxRetransmits: 0 });
  dc.binaryType = 'arraybuffer';
  return dc;
}

// Strips every ICE candidate line from an SDP: the hard side's answer advertises no
// address, so the easy side reaches it only through the sprayed guesses (as a real
// cone NAT only lets in a port it has sprayed), and the hard side advertises none, so
// it cannot connect on its own and the spray is what completes the link.
// NB48b-3: lines are split the way Chrome's SDP parser splits them, on `\n` only, so a
// candidate line ending in `\r\r\n`, a stray `\r` or a U+2028 is dropped like any other
// (a regex line model kept those, and Chrome still parsed them as candidates).
// MIC-1: gathering also writes the default candidate (a host one when no STUN server
// answered) into the connection line and the m-line port; those go back to the values
// before gathering, so the SDP names no address at all. ICE reads only candidate lines.
export function stripCandidates(sdp) {
  return sdp.split('\n').filter((l) => !/^\s*a=candidate:/i.test(l)).map(unaddressed).join('\n');
}
// An SDP line with the address gathering wrote into it put back: `c=`, the m-line port
// and `a=rtcp:` as before gathering (webrtc.js does the same for its descriptions).
function unaddressed(l) {
  return l.replace(/^c=IN IP[46] [^\s]+/i, 'c=IN IP4 0.0.0.0').replace(/^(m=\S+ )\d+/, (_, m) => `${m}9`)
    .replace(/^a=rtcp:\d+ IN IP[46] [^\s]+/i, 'a=rtcp:9 IN IP4 0.0.0.0');
}

// NB48b-3: Chrome's own view of a remote description we stripped must carry no
// candidate either; if one got through, fail rather than check it.
function assertNoCandidates(pc) {
  if (/a=candidate:/i.test(pc.remoteDescription?.sdp ?? '')) throw new Error("the peer's description still carries ICE candidates");
}

// The host UDP port an RTCPeerConnection gathered on, from its local description
// (its socket's port, which the far side's spray must hit). `null` until it gathers.
function hostPort(pc) {
  const m = /^a=candidate:\S+ \d+ udp \d+ (\d+\.\d+\.\d+\.\d+) (\d+) typ host/m.exec(pc.localDescription?.sdp ?? '');
  return m ? Number(m[2]) : null;
}

// The hard side (behind a symmetric NAT): opens `size` connections that share `cert`
// and `creds` (so the easy side answers them all with one answer), each answering the
// easy side's one `offerSdp`. Returns the one shared answer to send back (no
// candidates), the connections, a `ports()` reader for their sockets (which the easy
// side's spray must hit; in the real world they are the NAT's mappings, discovered by
// the punch, not shared), and `done`: the first connection whose channel opens, with
// the losers closed. `size` connections is heavy — the caller decides when to run it.
export async function openFleet({ RTCPeerConnection, cert, creds, offerSdp, size = FLEET_SIZE, easyAddr = null }) {
  // NB48-1: never check the candidates in the peer's offer — a hostile offer could
  // list many addresses (the victim's LAN, router, anywhere) and make every connection
  // in the fleet check all of them (256 × N). Strip them, cap the fleet, and check only
  // the one validated public address the easy side is known to be at, as native open does.
  size = Math.max(1, Math.min(size | 0, FLEET_SIZE));
  const offer = stripCandidates(offerSdp);
  const cand = easyCandidate(easyAddr);
  const conns = Array.from({ length: size }, () => {
    const pc = new RTCPeerConnection({ certificates: [cert] });
    return { pc, dc: dataChannel(pc) };
  });
  let answer = null;
  try {
    await Promise.all(conns.map(async ({ pc }) => {
      await pc.setRemoteDescription({ type: 'offer', sdp: offer });
      assertNoCandidates(pc);
      const ans = await pc.createAnswer();
      const munged = shareCredentials(ans.sdp, creds); // same ufrag/pwd on every connection
      await pc.setLocalDescription({ type: 'answer', sdp: munged });
      if (cand) await pc.addIceCandidate(cand).catch(() => {});
      answer ??= munged;
    }));
  } catch (e) {
    for (const c of conns) c.pc.close(); // no half-built fleet left behind
    throw e;
  }
  let settled = false;
  const done = new Promise((resolve) => {
    for (const c of conns) {
      c.dc.onopen = () => {
        if (settled) return;
        settled = true;
        for (const o of conns) if (o !== c) o.pc.close(); // keep the winner, drop the rest
        resolve(c);
      };
    }
  });
  return { answerSdp: stripCandidates(answer), conns, done, ports: () => conns.map((c) => hostPort(c.pc)).filter((p) => p != null) };
}

// The easy side (behind a cone that filters by port): one connection that offers to
// the hard side, then, given the hard side's one shared answer, sprays the hard side's
// `hardIp` at the guessed `ports`. A sprayed port that hits a hard socket completes the
// link: the hard side responds from that socket, and a cone only now lets it in,
// because the spray is the packet it just sent there. Returns the offer to send,
// `applyAnswer(sdp, hardIp, ports)`, and `done` (its own channel opening).
//
// With `advertise` (the default, the real world), the offer carries the easy side's
// own candidates — its public (STUN) address — so the hard side's fleet sends checks
// to it and opens its NAT mappings; the cone then blocks the hard side until the spray
// lands on a mapping. `iceServers` gives the STUN servers to gather that address.
// With `advertise` false (the loopback test), the offer carries no candidates, so the
// hard side cannot reach the easy side on its own and the spray is the only path.
export async function openSpray({ RTCPeerConnection, iceServers = [], advertise = true, gatherMs = 3000 }) {
  const pc = new RTCPeerConnection({ iceServers });
  const dc = dataChannel(pc);
  await pc.setLocalDescription(await pc.createOffer());
  // Advertising: wait for a public (srflx) candidate, or gathering to finish, or the cap.
  if (advertise && pc.iceGatheringState !== 'complete') {
    await new Promise((resolve) => {
      const done = () => { clearTimeout(t); pc.removeEventListener('icecandidate', onCand); resolve(); };
      const onCand = (e) => { if (!e.candidate || / typ srflx/.test(e.candidate.candidate)) done(); };
      const t = setTimeout(done, gatherMs);
      pc.addEventListener('icecandidate', onCand);
    });
  }
  const offerSdp = advertise ? pc.localDescription.sdp : stripCandidates(pc.localDescription.sdp);
  const done = new Promise((resolve) => { dc.onopen = () => resolve({ pc, dc }); });
  const applyAnswer = async (answerSdp, hardIp, ports) => {
    // NB48b-1: never check the candidates in the peer's answer — a hostile hard side
    // could list LAN or loopback addresses; only the sprayed guesses at `hardIp` count.
    try {
      await pc.setRemoteDescription({ type: 'answer', sdp: stripCandidates(answerSdp) });
      assertNoCandidates(pc);
    } catch (e) {
      pc.close();
      throw e;
    }
    for (const c of sprayCandidates(hardIp, ports)) await pc.addIceCandidate(c).catch(() => {});
  };
  return { offerSdp, pc, dc, done, applyAnswer };
}
