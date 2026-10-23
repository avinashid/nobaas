// NB-046: NAT detection from server-reflexive candidates, and the candidates added
// for a peer's port prediction. Pure functions: no browser, no network.
//
//   node packages/js/test/nat_predict.mjs
import { natFromSdp, predictedCandidates, checkPrediction, GUESS_BEFORE, GUESS_AFTER } from '../src/webrtc.js';

let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const srflx = (ip, port, n = 1) => `a=candidate:${n} 1 udp 1686052607 ${ip} ${port} typ srflx raddr 0.0.0.0 rport 0 generation 0`;
const host = 'a=candidate:9 1 udp 2122260223 192.168.1.5 50000 typ host generation 0';
const sdp = (...lines) => ['v=0', 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', ...lines].join('\r\n');
const eq = (a, b) => JSON.stringify(a) === JSON.stringify(b);

// Two STUN servers, two mappings 2 apart: sequential symmetric, next = 40004.
ok(eq(natFromSdp(sdp(host, srflx('203.0.113.7', 40000), srflx('203.0.113.7', 40002, 2))), { type: 'symmetric', ip: '203.0.113.7', next: 40004, step: 2 }), 'sequential symmetric: ip, next, step');
// Listed in either order (the two servers answer in any order).
ok(natFromSdp(sdp(srflx('203.0.113.7', 40002), srflx('203.0.113.7', 40001))).next === 40003, 'either order');
// Ports far apart: symmetric, no prediction.
ok(eq(natFromSdp(sdp(srflx('203.0.113.7', 40000), srflx('203.0.113.7', 51234))), { type: 'symmetric' }), 'random ports: symmetric, no prediction');
// One mapping (a cone, or one server): cone. None: unknown. IPv6 and host candidates don't count.
ok(natFromSdp(sdp(host, srflx('203.0.113.7', 40000))).type === 'cone', 'one mapping: cone');
ok(natFromSdp(sdp(host)).type === 'unknown', 'no srflx: unknown');
ok(natFromSdp(sdp(srflx('2001:db8::1', 4000), srflx('2001:db8::1', 4001))).type === 'unknown', 'IPv6 srflx ignored');
ok(natFromSdp(undefined).type === 'unknown' && natFromSdp('garbage').type === 'unknown', 'not an SDP: unknown');
// Candidate lines (an ice-batch) work as well as a description.
ok(natFromSdp([srflx('198.51.100.2', 1000), srflx('198.51.100.2', 1001)].join('\n')).next === 1002, 'candidate lines');
// Next past 65535: no prediction.
ok(natFromSdp(sdp(srflx('203.0.113.7', 65533), srflx('203.0.113.7', 65535))).next === undefined, 'no prediction past port 65535');

// Candidates for a prediction: 32 ports around `next`, at the peer's IP, low priority srflx.
const c = predictedCandidates({ ip: '203.0.113.7', next: 40004, step: 2 }, 'data');
ok(c.length === GUESS_BEFORE + GUESS_AFTER + 1, `${c.length} candidates`);
const ports = c.map((x) => Number(x.candidate.split(' ')[5]));
ok(ports[0] === 40004 - 2 * GUESS_BEFORE && ports.at(-1) === 40004 + 2 * GUESS_AFTER, `ports ${ports[0]} to ${ports.at(-1)}, step 2`);
ok(c.every((x) => / 203\.0\.113\.7 \d+ typ srflx /.test(x.candidate) && x.sdpMid === 'data' && x.sdpMLineIndex === 0), 'at the peer IP, typ srflx, the given mid');
ok(new Set(c.map((x) => x.candidate.split(' ')[0])).size === c.length, 'distinct foundations');
// Near the ends of the port range, out-of-range ports are left out.
ok(predictedCandidates({ ip: '203.0.113.7', next: 3, step: 1 }).length === GUESS_AFTER + 3, 'no port 0 or below');

// A peer's prediction is untrusted: only an IPv4 address and integers in range pass, so
// nothing can be spliced into a candidate line.
for (const bad of [
  null,
  'x',
  { ip: '203.0.113.7 9 typ host', next: 4000, step: 1 },
  { ip: '203.0.113.7\r\na=evil', next: 4000, step: 1 },
  { ip: '2001:db8::1', next: 4000, step: 1 },
  { ip: '256.0.0.1', next: 4000, step: 1 },
  { ip: '203.0.113.7', next: 0, step: 1 },
  { ip: '203.0.113.7', next: 65536, step: 1 },
  { ip: '203.0.113.7', next: 4000, step: 9 },
  { ip: '203.0.113.7', next: 4000, step: 0 },
  { ip: '203.0.113.7', next: '4000', step: 1 },
  { ip: '203.0.113.7', next: 4000.5, step: 1 },
]) {
  ok(checkPrediction(bad) === null && predictedCandidates(bad).length === 0, `refused: ${JSON.stringify(bad)}`);
}

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
