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
