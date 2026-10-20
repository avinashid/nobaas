// PERF-1: an endpoint never loses its timer. A transport that throws (dc.send on a
// full SCTP buffer) or refuses to send must not stop retransmits: a reliable message
// sent during the outage arrives after it, driven by the timer alone, long before the
// 10 s keepalive would have woken the endpoint up.
import { loadNobaas, Lane } from '../src/nobaas.js';
import { WebRtcTransport, MAX_BUFFERED } from '../src/webrtc.js';

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
// Before the fix a throwing transport escaped from the endpoint's microtask or timer.
let escaped = 0;
process.on('uncaughtException', () => escaped++);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const OUTAGE_MS = 1000; // long enough for several timer-driven retransmits to fail too
const DEADLINE_MS = 4000; // well inside the 10 s keepalive

// Sends `text` from a to b during an outage that `fault(on)` switches on and off, then
// waits for it with no further calls on either endpoint.
async function acrossOutage(a, b, fault, text) {
  const got = new Promise((resolve) => {
    const prev = b.onEvent;
    b.onEvent = (e) => {
      prev(e);
      if (e.type === 'message' && new TextDecoder().decode(e.data) === text) resolve(performance.now());
    };
  });
  fault(true);
  a.send(b.id, text, Lane.Reliable);
  await sleep(OUTAGE_MS);
  fault(false);
  const t0 = performance.now();
  const t = await Promise.race([got, sleep(DEADLINE_MS).then(() => null)]);
  return t === null ? null : t - t0;
}

async function connected(a, b) {
  for (let i = 0; i < 200 && !(a.isConnected(b.id) && b.isConnected(a.id)); i++) await sleep(5);
  return a.isConnected(b.id) && b.isConnected(a.id);
}

// 1. Endpoint pair on an in-memory wire whose a->b side throws, then refuses.
{
  const a = nb.endpoint();
  const b = nb.endpoint();
  let mode = 'ok';
  a.onTransmit = (addr, bytes) => {
    if (mode === 'throw') throw new DOMException('send queue is full', 'OperationError');
    if (mode === 'ok') setImmediate(() => b.receive(1, bytes));
  };
  b.onTransmit = (addr, bytes) => setImmediate(() => a.receive(2, bytes));
  a.connect(2, b.peerId);
  check(await connected(a, b), 'endpoint pair connected');

  let ms = await acrossOutage(a, b, (on) => (mode = on ? 'throw' : 'ok'), 'after a throwing transport');
  check(ms !== null, `a transport that throws: message delivered ${ms?.toFixed(0)} ms after the outage, by retransmit`);
  check(a.transmitErrors >= 2 && escaped === 0, `throws caught and counted (${a.transmitErrors} transmitErrors, none escaped, incl. timer-driven retransmits)`);

  ms = await acrossOutage(a, b, (on) => (mode = on ? 'refuse' : 'ok'), 'after a refusing transport');
  check(ms !== null, `a transport that refuses: message delivered ${ms?.toFixed(0)} ms after the outage`);

  // A throwing onEvent handler doesn't cost the timer either.
  const prev = b.onEvent;
  b.onEvent = () => {
    throw new Error('app bug');
  };
  a.send(b.id, 'x', Lane.Reliable);
  await sleep(50);
  b.onEvent = prev;
  check(b.timer !== null, 'the timer stays armed after onEvent throws');
  check(a.isConnected(b.id) && b.isConnected(a.id), 'link still up');
  escaped = 0;
  a.close();
  b.close();
}

// 2. The same through WebRtcTransport, on fake DataChannels wired to each other.
{
  const a = nb.endpoint();
  const b = nb.endpoint();
  const fakeSig = () => ({ onHello: () => {}, onMessage() {}, send() {}, hello() {}, close() {} });
  class Pc {
    createDataChannel() {
      return { readyState: 'open', bufferedAmount: 0, close() {} };
    }
    close() {}
  }
  const ta = new WebRtcTransport(a, fakeSig(), { RTCPeerConnection: Pc });
  const tb = new WebRtcTransport(b, fakeSig(), { RTCPeerConnection: Pc });
  const la = ta.link(b.id);
  const lb = tb.link(a.id);
  let sends = 0;
  let throwing = false;
  la.dc.send = (bytes) => {
    sends++;
    if (throwing) throw new DOMException('RTCDataChannel send queue is full', 'OperationError');
    const copy = bytes.slice().buffer;
    setImmediate(() => lb.dc.onmessage({ data: copy }));
  };
  lb.dc.send = (bytes) => {
    const copy = bytes.slice().buffer;
    setImmediate(() => la.dc.onmessage({ data: copy }));
  };
  a.connect(la.handle, b.peerId);
  check(await connected(a, b), 'WebRTC pair connected over fake DataChannels');

  let ms = await acrossOutage(a, b, (on) => (throwing = on), 'after dc.send threw');
  check(ms !== null, `dc.send throwing: message delivered ${ms?.toFixed(0)} ms after the outage`);
  check(ta.congested >= 2 && a.transmitErrors === 0 && escaped === 0, `the transport caught and counted it (congested ${ta.congested})`);

  const before = ta.congested;
  let sendsDuring = 0;
  ms = await acrossOutage(a, b, (on) => {
    la.dc.bufferedAmount = on ? MAX_BUFFERED + 1 : 0;
    if (on) sendsDuring = sends;
    else sendsDuring = sends - sendsDuring;
  }, 'after a full send buffer');
  check(sendsDuring === 0 && ta.congested > before, `over ${MAX_BUFFERED} B buffered: dc.send not called, ${ta.congested - before} datagrams counted`);
  check(ms !== null, `full send buffer: message delivered ${ms?.toFixed(0)} ms after it drained`);
  check(a.isConnected(b.id) && b.isConnected(a.id), 'WebRTC link still up');
  ta.close();
  tb.close();
}

console.log(failed ? `${failed} FAILED` : 'pump re-arm: all passed');
process.exit(failed ? 1 : 0);
