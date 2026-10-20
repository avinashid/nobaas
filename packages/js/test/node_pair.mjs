// Two endpoints in one Node process over an in-memory "wire": checks the JS glue.
import { loadNobaas, Lane } from '../src/nobaas.js';

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
const a = nb.endpoint();
const b = nb.endpoint();
// Handles: a sees b as 2, b sees a as 1.
a.onTransmit = (addr, bytes) => setImmediate(() => b.receive(1, bytes));
b.onTransmit = (addr, bytes) => setImmediate(() => a.receive(2, bytes));
const got = [];
let connected = 0;
const done = new Promise((resolve) => {
  b.onEvent = (e) => {
    if (e.type === 'connected') connected++;
    if (e.type === 'message') {
      got.push(e.data.slice());
      if (got.length === 1001) resolve();
    }
  };
});
a.onEvent = (e) => { if (e.type === 'connected') connected++; };
const t0 = performance.now();
a.connect(2, b.peerId);
while (!a.isConnected(b.id)) await new Promise((r) => setImmediate(r));
const tConn = performance.now() - t0;
const t1 = performance.now();
const msg = new Uint8Array(64).fill(7);
for (let i = 0; i < 1000; i++) {
  while (a.send(b.id, msg, Lane.Reliable) === -3) await new Promise((r) => setImmediate(r));
}
const big = crypto.getRandomValues(new Uint8Array(65536));
a.send(b.id, big, Lane.Reliable);
await done;
const dt = performance.now() - t1;
const ok = got[1000].length === big.length && got[1000].every((v, i) => v === big[i]);
console.log(`connected in ${tConn.toFixed(1)} ms; 1000 x 64 B + 64 KiB delivered in ${dt.toFixed(1)} ms; big message intact: ${ok}; connected events ${connected}; rtt ${a.rttMs(b.id)} ms`);
a.close();
b.close();
process.exit(ok && connected === 2 ? 0 : 1);
