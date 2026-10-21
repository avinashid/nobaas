// NB-014: fuzzes the JS package's parsers with seeded random and mutated
// inputs, in Node (no browser, no network):
// - decodeEvent, against a reference encoder of the documented event format;
// - toHex/fromHex, isPeerId, peerBytes, and Nostr's b64/unb64;
// - NostrSignaling.onRelay: mutated real hellos and sealed messages, and JSON
//   that isn't an array;
// - RoomProof.checkHello with hostile meta;
// - Room.onMessage: random frames from a proven member and from strangers,
//   after which the two rooms still talk.
// Nothing may throw out of a handler of untrusted input, and no promise may be
// left rejected.
//
//   node packages/js/test/fuzz.mjs        PW_FUZZ_SEED=7 PW_FUZZ_SCALE=20 for a soak
import { joinRoom, loadPeerwire, decodeEvent, toHex, fromHex, isPeerId, peerBytes, EventKind } from '../src/index.js';
import { NostrSignaling, b64, unb64 } from '../src/nostr.js';
import { RoomProof } from '../src/proof.js';

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const SEED = Number(process.env.PW_FUZZ_SEED ?? 0x6e62);
const SCALE = Math.max(1, Number(process.env.PW_FUZZ_SCALE ?? 1));
const cases = (n) => n * SCALE;

let failed = 0;
let mark = performance.now();
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what} [${(performance.now() - mark).toFixed(0)} ms]`);
  mark = performance.now();
  if (!ok) failed++;
};
const unhandled = [];
process.on('unhandledRejection', (e) => unhandled.push(e));

// sfc32, seeded per target: the same seed replays the same cases.
function rng(name) {
  let [a, b, c, d] = [SEED >>> 0, 0x9e3779b9, 0x243f6a88, 0xb7e15162];
  for (const ch of name) a = Math.imul(a ^ ch.charCodeAt(0), 0x01000193) >>> 0;
  const next = () => {
    const t = (((a + b) | 0) + d) | 0;
    d = (d + 1) | 0;
    a = b ^ (b >>> 9);
    b = (c + (c << 3)) | 0;
    c = (c << 21) | (c >>> 11);
    c = (c + t) | 0;
    return t >>> 0;
  };
  for (let i = 0; i < 16; i++) next();
  const r = {
    u32: next,
    below: (n) => next() % n,
    oneIn: (n) => next() % n === 0,
    pick: (xs) => xs[next() % xs.length],
    bytes: (n) => Uint8Array.from({ length: n }, () => next() & 0xff),
    // One to four of: bit flips, random or boundary bytes, inserts, deletes, duplicated chunks, truncation.
    mutate(b, max = 4096) {
      let v = Array.from(b);
      for (let k = 1 + r.below(4); k > 0; k--) {
        const at = r.below(v.length + 1);
        switch (r.below(6)) {
          case 0: if (at < v.length) v[at] ^= 1 << r.below(8); break;
          case 1: if (at < v.length) v[at] = r.pick([0, 1, 0x22, 0x5b, 0x7b, 0x7f, 0x80, 0xff, next() & 0xff]); break;
          case 2: v.splice(at, 0, ...r.bytes(1 + r.below(8))); break;
          case 3: v.splice(at, 1 + r.below(16)); break;
          case 4: if (v.length) { const f = r.below(v.length); v.splice(at, 0, ...v.slice(f, f + 1 + r.below(32))); } break;
          default: v.length = at;
        }
      }
      return Uint8Array.from(v.slice(0, max));
    },
  };
  return r;
}

const enc = new TextEncoder();
const dec = new TextDecoder();
const pw = await loadPeerwire(wasm);

// 1. decodeEvent: every documented event decodes to its fields; any other bytes
// decode to something or throw a RangeError (only the wasm produces events, and
// the Rust fuzzers check it emits the documented encoding).
{
  const r = rng('event');
  let n = 0, odd = 0;
  for (let i = 0; i < cases(20000); i++) {
    const kind = 1 + r.below(6), peer = r.bytes(32);
    const path = r.oneIn(2) ? [0, ...r.bytes(4)] : [1, ...r.bytes(32)];
    const body = {
      [EventKind.Connected]: [r.below(2), ...path],
      [EventKind.Message]: [r.below(2), ...r.bytes(r.below(200))],
      [EventKind.Disconnected]: [r.below(4)],
      [EventKind.ConnectFailed]: [],
      [EventKind.PathChanged]: path,
      [EventKind.AddrObserved]: r.bytes(4),
    }[kind];
    const b = Uint8Array.from([kind, ...peer, ...body]);
    const e = decodeEvent(b);
    const dv = new DataView(b.buffer);
    const want = (p, o) => (p[0] === 0 ? { direct: dv.getUint32(o + 1, true) } : { via: toHex(b.subarray(o + 1, o + 33)) });
    const ok = e.peer === toHex(peer) && {
      1: () => e.type === 'connected' && e.initiator === (body[0] === 1) && JSON.stringify(e.path) === JSON.stringify(want(path, 34)),
      2: () => e.type === 'message' && e.lane === body[0] && e.data.join() === body.slice(1).join(),
      3: () => e.type === 'disconnected' && e.reason === ['timeout', 'closed', 'replaced', 'violation'][body[0]],
      4: () => e.type === 'connectFailed',
      5: () => e.type === 'pathChanged' && JSON.stringify(e.path) === JSON.stringify(want(path, 33)),
      6: () => e.type === 'addrObserved' && e.addr === dv.getUint32(33, true),
    }[kind]();
    if (!ok) {
      check(false, `event ${toHex(b)} decoded as ${JSON.stringify(e)}`);
      break;
    }
    try {
      decodeEvent(r.mutate(b, 300));
    } catch (err) {
      if (!(err instanceof RangeError)) {
        check(false, `decodeEvent threw ${err}`);
        break;
      }
      odd++;
    }
    n++;
  }
  check(n === cases(20000), `decodeEvent: ${n} documented events decode exactly; mutated ones throw only RangeError (${odd})`);
}

// 2. Hex, peer ids and base64.
{
  const r = rng('text');
  let bad = 0;
  for (let i = 0; i < cases(20000); i++) {
    const b = r.bytes(r.below(80));
    if (fromHex(toHex(b)).join() !== b.join()) bad++;
    const s = r.oneIn(2) ? toHex(r.bytes(32)) : dec.decode(r.mutate(enc.encode(toHex(r.bytes(32))), 80));
    let p = null;
    try {
      p = peerBytes(s);
    } catch (err) {
      if (!(err instanceof TypeError)) bad++;
    }
    if (isPeerId(s) !== (p !== null) || (p && (p.length !== 32 || toHex(p) !== s))) bad++;
    const big = r.bytes(r.oneIn(50) ? 2000 : r.below(300)); // large bodies: nostr_bounds.mjs
    if (unb64(b64(big)).join() !== big.join()) bad++;
    try {
      const d = unb64(dec.decode(r.mutate(enc.encode(b64(b)), 200)));
      if (!(d instanceof Uint8Array)) bad++;
    } catch (err) {
      if (err?.name !== 'InvalidCharacterError') bad++;
    }
  }
  check(bad === 0, `hex, peer ids and base64: ${cases(20000)} cases round-trip, bad input only throws TypeError/InvalidCharacterError`);
}

// 3. Nostr relay messages, fed straight to onRelay (no relays).
{
  const r = rng('relay');
  const other = pw.endpoint();
  const sig = new NostrSignaling(pw, pw.endpoint(), { room: 'fuzz/relay', relays: [] });
  await sig.ready;
  const hellos = [], bodies = [];
  sig.onHello = (from, meta) => hellos.push([from, meta]);
  sig.onMessage((from, body) => bodies.push([from, body]));
  const threw = new Map(); // error -> [count, an input]
  const id = () => toHex(r.bytes(32));
  for (let i = 0; i < cases(6000); i++) {
    let raw;
    switch (r.below(5)) {
      case 0: raw = r.pick(['null', '1', '"x"', 'true', '[]', '{}', '[null]', '["EVENT"]', '["EVENT","s",null]', '["OK",1,false]', '['.repeat(5000)]); break;
      default: {
        const hello = { t: 'hello', from: r.oneIn(4) ? toHex(r.bytes(r.below(40))) : other.id, meta: r.pick([{}, null, 5, { role: 'peer' }]) };
        const msg = { kind: r.pick(['offer', 'answer', 'ice', 'dg']), sdp: { type: 'offer', sdp: 'v=0' }, n: r.u32() };
        const room = r.oneIn(2);
        const content = room ? b64(enc.encode(JSON.stringify(hello))) : b64(other.seal(sig.id, enc.encode(JSON.stringify(msg))));
        const ev = { id: id(), pubkey: 'ff'.repeat(32), created_at: r.u32(), kind: 25059, tags: [['p', room ? sig.roomTag : sig.myTag]], content, sig: '00' };
        raw = JSON.stringify(['EVENT', 's', ev]);
        if (r.below(3) !== 0) raw = dec.decode(r.mutate(enc.encode(raw), 8192));
      }
    }
    try {
      sig.onRelay('fuzz', raw);
    } catch (err) {
      const [n, input] = threw.get(err.message) ?? [0, raw.slice(0, 60)];
      threw.set(err.message, [n + 1, input]);
    }
  }
  const why = [...threw].map(([m, [n, input]]) => `${n}x ${m} on ${JSON.stringify(input)}`).join(' | ');
  check(threw.size === 0, `onRelay: ${cases(6000)} messages, none throws${why ? ` (${why})` : ''}`);
  check(hellos.length > 0 && hellos.every(([f]) => isPeerId(f) && f !== sig.id), `onRelay: ${hellos.length} hellos passed on, all from a PeerId`);
  check(bodies.length > 0 && bodies.every(([f]) => f === other.id), `onRelay: ${bodies.length} sealed messages passed on, all from their real sender`);
  sig.close();
}

// 4. Hello proofs with hostile meta: a boolean, never a throw.
{
  const r = rng('proof');
  const proof = await RoomProof.create('fuzz/room');
  const self = toHex(r.bytes(32));
  const good = await proof.hello(self);
  let bad = 0, accepted = 0, wrong = 0;
  const values = [undefined, null, '', 'peer', 'host', 'client', 'admin', 0, -1, 1.5, NaN, Infinity, 2 ** 53, -(2 ** 60), [], {}, '7'];
  for (let i = 0; i < cases(3000); i++) {
    const meta = r.oneIn(3)
      ? { ...good, [r.pick(['role', 'ts', 'p'])]: r.pick(values) }
      : r.pick([good, null, undefined, 5, 'x', [], { role: r.pick(values), ts: r.pick([...values, Date.now()]), p: r.pick([good.p, toHex(r.bytes(32)), ...values]) }]);
    const from = r.oneIn(4) ? r.pick(values) : self;
    try {
      const ok = await proof.checkHello(from, meta);
      if (typeof ok !== 'boolean') bad++;
      const real = from === self && meta?.p === good.p && meta?.ts === good.ts && meta?.role === good.role;
      accepted += ok;
      wrong += ok && !real;
    } catch {
      bad++;
    }
  }
  check(bad === 0 && accepted > 0 && wrong === 0, `checkHello: ${cases(3000)} metas, only the real proof is accepted (${accepted}, ${wrong} wrongly), none throws`);
}

// 5. Room frames from a proven member and from strangers.
{
  const r = rng('room');
  const opts = { appId: 'peerwire-fuzz', roomId: `fuzz-${process.pid}` };
  const [a, b] = await Promise.all([joinRoom(opts), joinRoom(opts)]);
  const t = Date.now();
  while (a.peers.length !== 1 || b.peers.length !== 1) {
    if (Date.now() - t > 10000) break;
    await new Promise((res) => setTimeout(res, 10));
  }
  check(a.peers.length === 1, 'room: two peers joined');
  const got = [];
  a.on('message', (data, from, info) => got.push([data, from, info.kind]));
  let threw = 0;
  const strangers = Array.from({ length: 4 }, () => toHex(r.bytes(32)));
  for (let i = 0; i < cases(5000); i++) {
    const tag = r.pick([0, 1, 2, 3, 5, 6, 255]); // never 4 (Bye) from b: it would leave
    const body = r.pick([
      () => r.bytes(r.below(200)),
      () => enc.encode(JSON.stringify(r.pick([null, 1, 'x', [1, 2], { a: { b: [] } }, { name: 'z' }]))),
      () => r.mutate(enc.encode('{"name":"bo","x":[1,2,3]}')),
      () => enc.encode('['.repeat(r.below(3000))),
    ])();
    const data = Uint8Array.from([tag, ...body]);
    const from = r.oneIn(3) ? r.pick(strangers) : b.self;
    try {
      a.onMessage(from, data, r.below(2));
    } catch {
      threw++;
    }
  }
  await new Promise((res) => setTimeout(res, 300)); // proof checks of strangers' frames settle
  check(threw === 0, `room: ${cases(5000)} frames, none throws`);
  check(got.length > 0 && got.every(([, f, k]) => f === b.self && ['bytes', 'text', 'json'].includes(k)), `room: ${got.length} messages, all from the member`);
  got.length = 0;
  b.send('still here');
  const t2 = Date.now();
  while (!got.some(([d]) => d === 'still here') && Date.now() - t2 < 5000) await new Promise((res) => setTimeout(res, 10));
  check(got.some(([d]) => d === 'still here') && a.peers.includes(b.self), 'room: the member still reaches us afterwards');
  await Promise.all([a.leave(), b.leave()]);
}

check(unhandled.length === 0, `no unhandled rejections${unhandled.length ? `: ${unhandled.slice(0, 3).map((e) => e?.message ?? e).join(' | ')}` : ''}`);
console.log(failed ? `${failed} FAILED (seed ${SEED})` : `fuzz: all passed (seed ${SEED}, scale ${SCALE})`);
process.exit(failed ? 1 : 0);
