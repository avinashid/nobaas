// A message sent to a newcomer from a 'peer-join' handler reaches it. A peer lists us once
// our presence arrives and drops anything from us before it. When the newcomer's presence
// came while its link proof was still being checked, the room used to emit 'peer-join'
// before our own presence had gone to it, so a welcome sent from the handler overtook our
// presence and was dropped: about 1 join in 5. Now held frames wait for our presence.
//
// 1. Held presence every time: the member with the smaller id checks link proofs 100 ms late,
//    so the other's presence always arrives during the check. 20 joins, both welcomes arrive.
// 2. Two members join as they come, 200 times: every welcome arrives.
// 3. A third member joins a pair, 30 times: all six welcomes arrive.
// A part stops at 10 lost welcomes, so a failing run ends soon.
//
//   node packages/js/test/room_welcome.mjs [joins]
import { joinRoom } from '../src/index.js';
import { RoomProof } from '../src/proof.js';

const JOINS = Number(process.argv[2] ?? 200);
const WAIT_MS = 1500; // a welcome that hasn't arrived by then was dropped
const STOP_AT = 10;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

let room = 0;
const rooms = new WeakMap(); // proof -> its room
const join = async () => {
  const r = await joinRoom({ appId: 'welcome', roomId: `welcome-${process.pid}-${room}` });
  rooms.set(r.proof, r);
  return r;
};
// Every member welcomes each peer that joins, with a little state, sent to that peer only.
// `got` collects "receiver<-sender" for each welcome that arrives from its sender.
const welcoming = (rs, got) => {
  for (const r of rs) {
    r.on('peer-join', (peer) => r.send({ welcome: r.self, members: r.peers.length }, { to: peer }));
    r.on('message', (d, peer) => d?.welcome === peer && got.add(`${r.self}<-${peer}`));
  }
  // A member that joined before its handlers were on sent no welcome: the join can't count.
  return rs.some((r) => r.peers.length);
};
const until = async (done) => {
  for (const t = Date.now(); !done() && Date.now() - t < WAIT_MS;) await sleep(2);
};

// `n` times, two members join a new room at once and welcome each other.
const pairs = async (n) => {
  let lost = 0, early = 0, runs = 0;
  const t = Date.now();
  for (; runs < n && lost < STOP_AT; runs++) {
    room++;
    const rs = await Promise.all([join(), join()]);
    const got = new Set();
    if (welcoming(rs, got)) early++;
    else {
      await until(() => got.size === 2);
      lost += 2 - got.size;
    }
    for (const r of rs) r.leave();
  }
  return { lost, runs, early, sent: 2 * (runs - early), ms: Date.now() - t };
};
const report = ({ lost, runs, early, sent, ms }, n) =>
  `${lost} of ${sent} welcomes lost in ${runs} joins${runs < n ? ` (stopped at ${STOP_AT})` : ''}${early ? `, ${early} too early to count` : ''}, ${ms} ms`;

// 1. The smaller id's link-proof checks finish 100 ms late. `held` counts the checks that
// finished with the peer's frames waiting, so the race really ran.
const checkPair = RoomProof.prototype.checkPair;
let held = 0;
RoomProof.prototype.checkPair = async function (label, from, to, proof) {
  const ok = await checkPair.call(this, label, from, to, proof);
  if (label !== 'pw-link' || to > from) return ok;
  await sleep(100);
  if (rooms.get(this)?.proving.get(from)?.length) held++;
  return ok;
};
let r = await pairs(20);
RoomProof.prototype.checkPair = checkPair;
check(held >= r.runs - 2, `a late link-proof check found the peer's presence waiting in ${held} of ${r.runs} joins`);
check(r.lost === 0 && !r.early, `held presence: ${report(r, 20)}`);

// 2. As they come.
r = await pairs(JOINS);
check(r.lost === 0 && r.early < JOINS / 10, `as they come: ${report(r, JOINS)}`);

// 3. A third member joins a pair: the pair welcomes it, and it welcomes both.
let lost = 0, early = 0, runs = 0;
const t = Date.now();
for (; runs < 30 && lost < STOP_AT; runs++) {
  room++;
  const got = new Set();
  const [x, y] = await Promise.all([join(), join()]);
  if (welcoming([x, y], got)) early++;
  await until(() => got.size === 2);
  const z = await join();
  if (welcoming([z], got)) early++;
  await until(() => got.size === 6);
  lost += 6 - got.size;
  for (const m of [x, y, z]) m.leave();
}
check(lost === 0 && !early, `a third member: ${lost} of ${6 * runs} welcomes lost in ${runs} joins${early ? `, ${early} too early` : ''}, ${Date.now() - t} ms`);

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
