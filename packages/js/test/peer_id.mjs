// NB-021 (SEC-1): peer ids from untrusted signalling must never reach wasm memory
// unchecked. The repro from the card (an over-long id to seal) must throw, the
// module must keep working afterwards, and every signalling layer must drop a
// `from` that isn't a PeerId.
import { loadNobaas, isPeerId, peerBytes } from '../src/nobaas.js';
import { BroadcastSignaling, WebRtcTransport } from '../src/webrtc.js';
import { LocalTransport } from '../src/local.js';
import { NostrSignaling } from '../src/nostr.js';
import { Room } from '../src/room.js';

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const throws = (fn, what) => {
  try {
    fn();
  } catch (e) {
    return check(e instanceof TypeError, `${what} throws (${e.message})`);
  }
  check(false, `${what} throws`);
};
const tick = () => new Promise((r) => setTimeout(r, 20));

const ep = nb.endpoint();
const other = nb.endpoint();
const BAD = {
  'the card repro (8192 hex chars)': 'ab'.repeat(4096),
  '66 hex chars': 'ab'.repeat(33),
  '62 hex chars': 'ab'.repeat(31),
  'uppercase hex': 'AB'.repeat(32),
  'non-hex': 'zz'.repeat(32),
  '33 bytes': new Uint8Array(33),
  '31 bytes': new Uint8Array(31),
  '4096 bytes': new Uint8Array(4096),
  'a plain array': Array(32).fill(1),
  'a number': 42,
  'null': null,
};

// 1. The repro from the card, then the module still works.
throws(() => ep.seal('ab'.repeat(4096), new Uint8Array([1, 2, 3])), 'seal with the card repro id');
const fresh = nb.endpoint(); // trapped with RuntimeError: unreachable before the fix
check(isPeerId(fresh.id), 'a new endpoint after the repro');
const sealed = ep.seal(other.id, new Uint8Array([1, 2, 3]));
const opened = other.open(sealed);
check(opened && opened.from === ep.id && opened.data.join() === '1,2,3', 'seal/open round trip after the repro');

// 2. Every entry point that copies a peer id rejects anything but 32 bytes / 64 lowercase hex.
for (const [name, id] of Object.entries(BAD)) {
  for (const [method, call] of [
    ['setPeer', () => ep.setPeer(id)],
    ['seal', () => ep.seal(id, new Uint8Array(3))],
    ['connect', () => ep.connect(1, id)],
    ['connectVia (peer)', () => ep.connectVia(other.id, id)],
    ['connectVia (relay)', () => ep.connectVia(id, other.id)],
    ['send', () => ep.send(id, 'x')],
    ['isConnected', () => ep.isConnected(id)],
    ['rttMs', () => ep.rttMs(id)],
    ['peerBytes', () => peerBytes(id)],
  ]) {
    try {
      call();
      check(false, `${method}(${name}) throws`);
    } catch (e) {
      if (!(e instanceof TypeError)) check(false, `${method}(${name}) throws a TypeError, got ${e}`);
    }
  }
}
check(failed === 0, `9 entry points x ${Object.keys(BAD).length} bad ids all throw a TypeError`);
check(ep.isConnected(other.peerId) === false && ep.isConnected(other.id) === false, 'valid ids (bytes and hex) still accepted');

// 3. Other fixed-size copies.
throws(() => nb.endpoint({ secret: new Uint8Array(4096) }), 'endpoint with a 4096-byte secret');
throws(() => nb.endpoint({ secret: new Uint8Array(16) }), 'endpoint with a 16-byte secret');
throws(() => nb.nostrSign(new Uint8Array(4096), new Uint8Array(32)), 'nostrSign with a 4096-byte secret');
throws(() => nb.nostrSign(new Uint8Array(32), new Uint8Array(64)), 'nostrSign with a 64-byte message');
throws(() => nb.put('not bytes'), 'put with a string');
throws(() => ep.open({ length: 5 }), 'open with a fake array-like');
const same = nb.endpoint({ secret: ep.secret() });
check(same.id === ep.id, 'a 32-byte secret restores the identity');

// 4. Signalling: hostile `from` values are dropped before anything uses them.
const hostile = ['ab'.repeat(4096), 'AB'.repeat(32), '', 42, null, undefined, { id: 'ab'.repeat(32) }, ['ab'.repeat(32)]];

// BroadcastSignaling (BroadcastChannel exists in Node).
{
  const sig = new BroadcastSignaling(ep.id, 'nb021-test');
  const seen = [];
  sig.onHello = (from) => seen.push(['hello', from]);
  sig.onMessage((from) => seen.push(['msg', from]));
  const attacker = new BroadcastChannel('nb021-test');
  for (const from of hostile) {
    attacker.postMessage({ kind: 'hello', from, meta: {} });
    attacker.postMessage({ kind: 'msg', from, to: ep.id, body: { kind: 'offer' } });
  }
  attacker.postMessage(null);
  attacker.postMessage({ kind: 'hello', from: other.id, meta: {} });
  await tick();
  check(seen.length === 1 && seen[0][1] === other.id, `BroadcastSignaling drops ${hostile.length} bad froms, keeps a good one`);
  throws(() => sig.send('ab'.repeat(4096), {}), 'BroadcastSignaling.send to a bad id');
  attacker.close();
  sig.close();
}

// A fake signalling adapter to drive transports and rooms directly.
const fakeSig = () => ({ handlers: [], sent: [], onHello: () => {}, onMessage(fn) { this.handlers.push(fn); }, send(to, m) { this.sent.push([to, m]); }, hello() {}, close() {} });

// WebRtcTransport: no RTCPeerConnection is made for a bad `from`.
{
  let made = 0;
  class Pc {
    constructor() { made++; }
    createDataChannel() { return {}; }
    close() {}
  }
  const sig = fakeSig();
  const t = new WebRtcTransport(nb.endpoint(), sig, { RTCPeerConnection: Pc });
  for (const from of hostile) for (const kind of ['offer', 'answer', 'ice']) await sig.handlers[0](from, { kind, sdp: {} });
  await sig.handlers[0](other.id, null);
  check(made === 0 && t.links.size === 0, 'WebRtcTransport makes no link for bad froms');
  let rejected = false;
  await t.dial('ab'.repeat(4096)).catch((e) => (rejected = e instanceof TypeError));
  check(rejected && made === 0, 'WebRtcTransport.dial rejects a bad id');
  throws(() => new WebRtcTransport(nb.endpoint(), fakeSig(), { RTCPeerConnection: Pc, privacy: 'relay-only', relay: 'ab'.repeat(4096) }), 'relay-only with a bad relay id');
}

// LocalTransport: datagrams from a bad `from` never reach the endpoint.
{
  const sig = fakeSig();
  const e2 = nb.endpoint();
  const t = new LocalTransport(e2, sig);
  for (const from of hostile) sig.handlers[0](from, { kind: 'dg', bytes: new Uint8Array(40) });
  check(t.byPeer.size === 0 && e2.stats().rxPackets === 0, 'LocalTransport drops datagrams from bad froms');
  throws(() => t.dial('ab'.repeat(4096)), 'LocalTransport.dial with a bad id');
}

// NostrSignaling: a hello with a bad `from` in a well-formed relay event is dropped.
{
  const sig = new NostrSignaling(nb, nb.endpoint(), { room: 'nb021/test', relays: [] });
  await sig.ready;
  const seen = [];
  sig.onHello = (from) => seen.push(from);
  let n = 0;
  const event = (from) => ['EVENT', 'sub', { id: `e${n++}`, kind: 25059, pubkey: 'x', tags: [['p', sig.roomTag]], content: btoa(JSON.stringify({ t: 'hello', from, meta: {} })) }];
  for (const from of hostile) sig.onRelay('test', JSON.stringify(event(from)));
  sig.onRelay('test', JSON.stringify(event(other.id)));
  check(seen.length === 1 && seen[0] === other.id, 'NostrSignaling drops hellos with bad froms, keeps a good one');
  let rejected = false;
  await sig.send('ab'.repeat(4096), {}).catch((e) => (rejected = e instanceof TypeError));
  check(rejected, 'NostrSignaling.send rejects a bad id');
  sig.close();
}

// Room: a custom signalling adapter passing junk to onHello dials nothing.
{
  const sig = fakeSig();
  const dialled = [];
  const room = new Room(nb.endpoint(), sig, () => ({ has: () => false, dial: (p) => dialled.push(p), drop() {}, close() {} }), {});
  for (const from of hostile) sig.onHello(from, {});
  check(dialled.length === 0, 'Room dials nothing for bad hello ids');
  await room.leave();
}

// 5. The module is still healthy: two fresh endpoints still seal to each other.
const x = nb.endpoint();
const y = nb.endpoint();
check(y.open(x.seal(y.id, new Uint8Array([9])))?.from === x.id, 'module still healthy at the end');

console.log(failed ? `${failed} FAILED` : 'peer id checks: all passed');
process.exit(failed ? 1 : 0);
