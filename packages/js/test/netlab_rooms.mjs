// Net Lab's named rooms (demo/netlab.html), in headless Chromium tabs on one
// machine (BroadcastChannel signalling, loopback WebRTC):
// - a room name as typed opens the room id it shows (roomIdOf);
// - typing a name and "Join room" joins it, and the address bar gets #room=<id>;
// - the room link opens the same room in another tab, which links and pings;
// - another name is another room: nobody from the first shows up there;
// - the name joins the recent rooms; the report never holds it.
//
//   node packages/js/test/netlab_rooms.mjs [screenshot-prefix]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shot = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const page = `http://127.0.0.1:${port}/demo/netlab.html?signal=local`;

try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
  const open = async (url) => {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => ok(false, `page error: ${e.message}`));
    await p.goto(url);
    return p;
  };
  const wait = (p, fn, arg, timeout = 30000) => p.waitForFunction(fn, arg, { timeout });

  const a = await open(page);
  const ids = await a.evaluate(() => ['Office Test!', '  Café  Lab ', '---', 'a'.repeat(60), 'Ünïcode 42'].map(window.netlab.roomIdOf));
  ok(ids[0] === 'office-test' && ids[1] === 'cafe-lab' && ids[2] === '' && ids[3].length === 40 && ids[4] === 'unicode-42', `room ids: ${JSON.stringify(ids)}`);
  const name = `Office Test ${port}`;
  const id = `office-test-${port}`;
  await a.fill('#name', 'alpha');
  await a.fill('#roomname', name);
  ok((await a.textContent('#roomhint')).includes(id), 'the hint shows the room id');
  if (shot) await a.screenshot({ path: `${shot}-join.png` });
  await a.click('#joinname');
  await wait(a, () => window.netlab.self && window.netlab.room?.kind === 'named');
  const room = await a.evaluate(() => ({ ...window.netlab.room, hash: location.hash }));
  ok(room.label === id && room.hash === `#room=${id}` && room.link.endsWith(`?signal=local#room=${id}`), `joined ${room.label}; address ${room.hash}, link ${room.link.split('/').pop()}`);
  ok((await a.textContent('#h-room')) === id, 'the header names the room');

  // The link opens the same room, with no typing.
  const b = await open(room.link.replace('#', '&name=bravo#'));
  await Promise.all([a, b].map((p) => wait(p, () => window.netlab.peers().length === 1 && window.netlab.peers()[0].ping.n >= 2)));
  ok(true, 'the link brought a second tab into the room: linked, pinging');

  // Another name, another room.
  const c = await open(page);
  await c.fill('#name', 'charlie');
  await c.fill('#roomname', `${name} b`);
  await c.press('#roomname', 'Enter');
  await wait(c, () => window.netlab.self);
  await c.waitForTimeout(3000);
  ok((await c.evaluate(() => window.netlab.peers().length)) === 0, 'another name is another room: no one from the first');

  // A <details> (candidate pairs / route history) the user opens stays open across
  // the 1 s re-render, instead of snapping shut.
  const det = await a.waitForFunction(() => document.querySelector('#peers details')?.dataset.d ?? null, null, { timeout: 15000 }).then((h) => h.jsonValue());
  await a.click(`#peers details[data-d="${det}"] summary`);
  await a.waitForTimeout(1500); // more than one render tick
  ok(await a.evaluate((d) => document.querySelector(`#peers details[data-d="${d}"]`)?.open === true, det), `an opened "${det.split('-')[0]}" panel stays open across re-renders`);

  const rep = await a.evaluate(() => JSON.stringify(window.netlab.report()));
  const at = rep.indexOf(id);
  ok(JSON.parse(rep).room.kind === 'named' && at < 0, `the report says named, without the name${at < 0 ? '' : `: …${rep.slice(at - 80, at + 40)}`}`);
  if (shot) {
    await a.waitForTimeout(2500);
    await a.screenshot({ path: `${shot}-room.png`, fullPage: true });
  }

  // Back at the join screen, the room is a recent one.
  await a.click('#leave');
  await wait(a, () => !window.netlab.self && document.querySelector('#recent [data-room]'));
  ok(await a.evaluate((r) => [...document.querySelectorAll('#recent [data-room]')].some((b) => b.dataset.room === r), id), 'recent rooms list it after leaving');
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
