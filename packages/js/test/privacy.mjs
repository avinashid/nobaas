// NB-034, in Node over an in-memory wire: SDP host-candidate filtering, and a
// relay-only endpoint linking only through a relay and refusing direct dials.
import { loadNobaas, Lane } from '../nobaas.js';
import { withoutHostCandidates } from '../webrtc.js';

const fail = (m) => {
  console.error(`FAIL: ${m}`);
  process.exit(1);
};

const sdp = ['v=0', 'a=candidate:1 1 udp 2122260223 192.168.1.5 50000 typ host generation 0',
  'a=candidate:2 1 udp 2122260223 4b1e7a2c.local 50001 typ host', 'a=candidate:3 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 0.0.0.0 rport 0',
  'a=mid:0'].join('\r\n');
const kept = withoutHostCandidates(sdp);
if (/typ host/.test(kept) || !/typ srflx/.test(kept) || !kept.includes('a=mid:0')) fail(`filtered SDP: ${kept}`);

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
// Three endpoints, every pair wired: handle n reaches endpoint n.
const eps = [nb.endpoint({ relayOnly: true }), nb.endpoint(), nb.endpoint()];
const [a, r, b] = eps;
eps.forEach((ep, i) => {
  ep.onTransmit = (to, bytes) => setImmediate(() => eps[to].receive(i, bytes));
});
const events = eps.map(() => []);
eps.forEach((ep, i) => {
  ep.onEvent = (e) => events[i].push(e);
});
const until = async (cond, ms = 3000) => {
  const end = performance.now() + ms;
  while (!cond() && performance.now() < end) await new Promise((res) => setTimeout(res, 1));
  return cond();
};
// A direct dial from B to relay-only A goes unanswered.
b.connect(0, a.peerId);
if (await until(() => b.isConnected(a.id), 700)) fail('a relay-only endpoint answered a direct dial');
a.connect(1, r.peerId);
b.connect(1, r.peerId);
if (!(await until(() => a.isConnected(r.id) && b.isConnected(r.id)))) fail('no links to the relay');
a.connectVia(r.peerId, b.peerId);
if (!(await until(() => a.isConnected(b.id) && b.isConnected(a.id)))) fail('no relayed link');
a.send(b.id, new TextEncoder().encode('psst'), Lane.Reliable);
if (!(await until(() => events[2].some((e) => e.type === 'message')))) fail('no message through the relay');
await until(() => false, 300);
const direct = events[0].concat(events[2]).filter((e) => e.type === 'pathChanged');
if (direct.length) fail(`the relayed link moved: ${JSON.stringify(direct)}`);
console.log(`privacy: SDP filtered (${kept.split('\r\n').length} lines kept); relay-only link up through the relay, direct dial refused, relayed ${r.stats().relayed} packets`);
eps.forEach((ep) => ep.close());
process.exit(0);
