// Endpoint.receive(addr, bytes, peer): `peer` is the only peer that sends on handle `addr`
// (its DataChannel, its signalling pipe), so a datagram there for another peer's link is
// dropped unread. A member that relays for others then can't send a packet of theirs it
// kept on its own channel to move their link onto it (FL2-2). Without a peer, as on UDP,
// a link roams to wherever its peer's packets come from. Checks:
// - one of b's datagrams to a, kept back, given to a on handle 3 naming c: no path change,
//   nothing decrypted or taken as a replay;
// - the same datagram on handle 3 with no peer: it was never read, so it is fresh, and a's
//   link to b roams to handle 3;
// - the link still carries messages both ways.
//
//   node packages/js/test/receive_from.mjs [path/to/peerwire_ffi.wasm]
import { loadPeerwire } from '../src/peerwire.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
const tick = () => new Promise((r) => setTimeout(r, 20));
const until = async (what, cond, ms = 5000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await tick();
  }
};

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
const [a, b, c] = [pw.endpoint(), pw.endpoint(), pw.endpoint()];
// a reaches b at handle 2 and b reaches a at handle 1, each naming the other as its peer;
// what a sends to any other handle is lost.
let keep = false;
let kept = null;
a.onTransmit = (addr, bytes) => addr === 2 && setImmediate(() => b.receive(1, bytes, a.id));
b.onTransmit = (addr, bytes) => {
  if (keep && !kept) return void (kept = bytes);
  setImmediate(() => a.receive(2, bytes, b.id));
};
const paths = [];
const got = [];
a.onEvent = (e) => {
  if (e.type === 'pathChanged') paths.push(e.path);
  if (e.type === 'message') got.push(['a', new TextDecoder().decode(e.data)]);
};
b.onEvent = (e) => e.type === 'message' && got.push(['b', new TextDecoder().decode(e.data)]);
a.connect(2, b.id);
await until('a and b linked', () => a.isConnected(b.id) && b.isConnected(a.id));

keep = true;
b.send(a.id, 'kept back');
await until("a datagram of b's, kept", () => kept !== null);
const before = a.stats();
a.receive(3, kept, c.id);
await tick();
const after = a.stats();
if (paths.length) fail(`b's datagram on c's handle moved the link: ${JSON.stringify(paths)}`);
if (after.rxPackets <= before.rxPackets) fail('the datagram on handle 3 was not received');
if (after.rxAuthFailed !== before.rxAuthFailed || after.rxReplayed !== before.rxReplayed) fail('it was read');
console.log("b's datagram on c's handle: dropped unread, no path change");

a.receive(3, kept);
await tick();
if (paths.length !== 1 || paths[0].direct !== 3) fail(`with no peer named, the link didn't roam to 3: ${JSON.stringify(paths)}`);
if (a.stats().rxReplayed !== before.rxReplayed) fail('the datagram was taken as a replay: the first one was read');
console.log('the same datagram with no peer named: fresh, and the link roams to handle 3 (as on UDP)');

// Back on handle 2, b's: a's link follows b's next datagram there.
keep = false;
a.send(b.id, 'to b');
b.send(a.id, 'to a');
await until('messages both ways', () => got.some(([w, m]) => w === 'a' && m === 'to a') && got.some(([w, m]) => w === 'b' && m === 'to b'));
console.log('messages flow both ways');
for (const e of [a, b, c]) e.close();
console.log('receive_from: ok');
process.exit(0);
