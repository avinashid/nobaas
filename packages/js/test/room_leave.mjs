// A leaving room must reach its peers at once even when its reliable send window is
// full, and must do it all before leave() returns: on pagehide the page may be gone
// before any timer or await. Three rooms in this process (BroadcastChannel and
// LocalTransport): a sends 6 MiB, then leaves at once. Then, with the Bye lost, the
// core's sealed close alone must still tell the other side. Last, the pagehide handler
// and its grace. test/leave.mjs does the same with real tab closes in Chromium.
//
//   node packages/js/test/room_leave.mjs
import { joinRoom, Lane } from '../src/index.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (cond, ms) => {
  for (const t = Date.now(); !cond() && Date.now() - t < ms;) await sleep(5);
  return cond();
};
const FAST_MS = 2000; // far below the 30 s idle_timeout

async function rooms(n, name) {
  const opts = { appId: 'peerwire-test', roomId: `leave-${name}-${process.pid}` };
  const rs = await Promise.all(Array.from({ length: n }, () => joinRoom(opts)));
  check(await until(() => rs.every((r) => r.peers.length === n - 1), 10000), `${n} rooms joined`);
  return rs;
}
const leaves = (r) => {
  const seen = [];
  r.on('peer-leave', (peer, reason) => seen.push({ peer, reason, at: performance.now() }));
  return seen;
};

// 1. A full send window: the Bye must not wait behind it.
{
  const [a, b, c] = await rooms(3, 'full');
  const seen = [b, c].map(leaves);
  for (let i = 0; i < 48; i++) a.send(new Uint8Array(128 << 10)); // 6 MiB to each, over the 4 MiB window
  const waiting = [...a.outbox.values()].reduce((n, q) => n + q.length, 0);
  check(waiting > 0, `the send window is full (${waiting} frames wait in the outbox)`);
  const t0 = performance.now();
  const p = a.leave();
  check(a.endpoint.closed && a.left, 'leave() said goodbye and closed before returning (no timer, no await)');
  check(await until(() => seen.every((s) => s.length), FAST_MS), `both peers saw the leave within ${FAST_MS} ms`);
  for (const [i, s] of seen.entries()) {
    check(s.length === 1 && s[0].peer === a.self && s[0].reason === 'left',
      `peer ${i + 1}: peer-leave ${s[0]?.reason} after ${s.length ? (s[0].at - t0).toFixed(0) : '-'} ms`);
  }
  await p;
  await Promise.all([b.leave(), c.leave()]);
}

// 2. The Bye is lost: the core's close (Endpoint.disconnect) still tells the peer.
{
  const [a, b] = await rooms(2, 'nobye');
  const seen = leaves(b);
  const send = a.endpoint.send.bind(a.endpoint);
  let dropped = 0;
  a.endpoint.send = (peer, data, lane) => (data.length === 1 && lane === Lane.Unreliable ? void dropped++ : send(peer, data, lane));
  const t0 = performance.now();
  a.leave();
  check(dropped === 1, 'the Bye was dropped');
  check(await until(() => seen.length, FAST_MS) && seen[0].peer === a.self && seen[0].reason === 'closed',
    `with no Bye the core's close reached the peer: peer-leave ${seen[0]?.reason} after ${seen.length ? (seen[0].at - t0).toFixed(0) : '-'} ms`);
  await b.leave();
}

// 3. pagehide: the handler says goodbye, then holds the page ~20 ms (the browser may end
// the renderer once it returns, before the Bye is on the wire), but only if it had peers.
{
  const [a, b] = await rooms(2, 'pagehide');
  const seen = leaves(b);
  let t0 = performance.now();
  a.onPageHide();
  const held = performance.now() - t0;
  check(a.endpoint.closed && held >= 20, `the pagehide handler left and held the page ${held.toFixed(1)} ms`);
  check(await until(() => seen.length, FAST_MS) && seen[0].reason === 'left', `the peer saw peer-leave ${seen[0]?.reason}`);
  t0 = performance.now();
  a.onPageHide();
  const again = performance.now() - t0;
  check(again < 5, `a second pagehide, with no peers left, returns at once (${again.toFixed(1)} ms)`);
  await b.leave();
}

console.log(failed ? `${failed} FAILED` : 'room leave: all passed');
process.exit(failed ? 1 : 0);
