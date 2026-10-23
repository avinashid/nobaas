// Net Lab's "Use microphone" button (demo/netlab.html), in headless Chromium with fake
// media and the browser's mDNS hiding left on, as on a real device:
// - before: the report says mediaPermission 'prompt' and the network test's local
//   addresses are hidden (.local);
// - the button: 'granted', the network test lists real local addresses (and reads the
//   same NAT, though each interface now has its own public port), the room is joined
//   again (a new id, so new connections), and the other tab sees mic allowed and this
//   tab's real local addresses on the new link; nothing throws;
// - the global room (MIC-1): with the grant stored before the tabs open, or given while
//   they are in it, the other tab gets no local address, and the pair still links;
// - a refused prompt (a browser whose fake UI says no): 'denied', a warning, and the
//   room is not joined again;
// - a candidate pair whose local address is hidden takes its family from the remote.
//
//   node packages/js/test/netlab_mic.mjs [screenshot-prefix]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shot = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const origin = `http://127.0.0.1:${port}`;
const roomUrl = (name) => `${origin}/demo/netlab.html?signal=local&name=${name}#room=mic-test-${port}`;
const browsers = [];
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const hidden = (a) => a.endsWith('.local');

try {
  await new Promise((r) => setTimeout(r, 600));
  const launch = async (ui) => {
    const b = await chromium.launch({ args: [ui, '--use-fake-device-for-media-stream'] });
    browsers.push(b);
    return b.newContext({ viewport: { width: 1360, height: 900 } });
  };
  const open = async (ctx, url) => {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => ok(false, `page error: ${e.message}`));
    await p.goto(url);
    return p;
  };
  const wait = (p, fn, arg, timeout = 45000) => p.waitForFunction(fn, arg, { timeout });
  const report = (p) => p.evaluate(() => window.netlab.report());
  const card = (p) => p.locator('#mynet').locator('..');

  const ctx = await launch('--use-fake-ui-for-media-stream');
  const a = await open(ctx, roomUrl('alpha'));
  const b = await open(ctx, roomUrl('bravo'));
  await Promise.all([a, b].map((p) => wait(p, () => window.netlab.peers().length === 1 && window.netlab.report().me.network)));
  const before = await report(a);
  const hosts0 = before.me.network.host.map((c) => c.address);
  ok(before.me.mediaPermission === 'prompt', `before: mediaPermission ${before.me.mediaPermission}`);
  ok(hosts0.length > 0 && hosts0.every(hidden), `before: local addresses hidden (${hosts0.join(', ')})`);
  const bBefore = (await report(b)).peers.find((p) => p.id === before.me.id);
  ok(bBefore?.presence?.mic === false, `before: the other tab sees mic ${bBefore?.presence?.mic}`);
  if (shot) await card(a).screenshot({ path: `${shot}-before.png` });

  // The fake UI says yes to the prompt but doesn't store the grant, as a tap on Allow
  // does (Chrome shows local addresses only with a stored grant), so store it too.
  await ctx.grantPermissions(['microphone'], { origin });
  await a.click('#mic');
  await wait(a, (id) => window.netlab.self && window.netlab.self !== id && window.netlab.peers().length === 1, before.me.id);
  const after = await report(a);
  const hosts1 = after.me.network.host.map((c) => c.address);
  ok(after.me.mediaPermission === 'granted', `after the button: mediaPermission ${after.me.mediaPermission}`);
  ok(after.me.network.at !== before.me.network.at && hosts1.length > 0 && !hosts1.some(hidden), `the network test ran again: local addresses ${hosts1.join(', ')}`);
  // Each interface now has its own socket and public port: still the same NAT reading.
  ok(after.me.network.nat === before.me.network.nat, `and reads the same NAT (${before.me.network.nat} → ${after.me.network.nat}, ${after.me.network.srflx.length} public candidates)`);
  ok(after.me.id !== before.me.id, `rejoined the room as a new member (${before.me.id.slice(0, 6)} → ${after.me.id.slice(0, 6)})`);
  ok(after.log.some((e) => e.msg.startsWith('microphone allowed')), 'the log says the microphone was allowed');
  // The other tab: the new link offers this tab's real local addresses.
  await wait(b, (id) => {
    const p = window.netlab.report().peers.find((q) => q.id === id);
    return p?.presence?.mic === true && p.theirHosts.length > 0 && p.route === 'direct';
  }, after.me.id).catch(() => {});
  const seen = (await report(b)).peers.find((p) => p.id === after.me.id);
  ok(seen?.presence?.mic === true, `the other tab sees mic ${seen?.presence?.mic}`);
  ok(seen?.theirHosts.length > 0 && !seen.theirHosts.some(hidden), `and the new link's local addresses: ${seen?.theirHosts.join(', ')}`);
  ok(seen?.route === 'direct', `and links direct (${seen?.route})`);
  const errors = [a, b].map(async (p) => (await report(p)).log.filter((e) => e.level === 'error').map((e) => e.msg));
  const errs = (await Promise.all(errors)).flat();
  ok(!errs.length, `nothing thrown or unhandled${errs.length ? `: ${errs.join(' | ')}` : ''}`);
  if (shot) {
    await card(a).screenshot({ path: `${shot}-after.png` });
    await b.screenshot({ path: `${shot}-peer.png`, fullPage: true });
  }

  // Local hidden, remote IPv6: the pair is IPv6 (it read host/v4 ↔ host/v6).
  const labels = await a.evaluate(() => [
    window.netlab.pairLabel({ candidateType: 'host', address: '' }, { candidateType: 'host', address: '2001:db8::1', port: 5000 }),
    window.netlab.pairLabel({ candidateType: 'host', address: 'abc.local' }, { candidateType: 'srflx', address: '2001:db8::2', port: 6000 }),
    window.netlab.pairLabel({ candidateType: 'host', address: '10.0.0.2' }, { candidateType: 'host', address: '2001:db8::3', port: 7000 }),
    window.netlab.pairLabel({ candidateType: 'host' }, { candidateType: 'prflx', address: '198.51.100.4', port: 8000 }),
  ]);
  ok(labels[0] === 'host/v6 ↔ host/v6 2001:db8::1:5000' && labels[1] === 'host/v6 ↔ srflx/v6 2001:db8::2:6000'
    && labels[2] === 'host/v4 ↔ host/v6 2001:db8::3:7000' && labels[3] === 'host/v4 ↔ prflx/v4 198.51.100.4:8000', `pair labels: ${labels.join(' | ')}`);

  // The global room, which anyone can open, never gets the local addresses (MIC-1).
  // G: the grant was stored on an earlier visit, so both tabs join without them.
  // H: the grant comes while both tabs are in the global room: they join it again
  // without them. Each pair still links (over signalling here: no STUN on loopback).
  const globalUrl = (name) => `${origin}/demo/netlab.html?signal=local&name=${name}&auto=global`;
  const linkedTo = (p, id) => wait(p, (x) => window.netlab.peers().some((q) => q.id === x), id, 60000);
  // Net Lab notes a link's local addresses 500 ms after the offer or answer: read them after that.
  const hostsOf = async (p, id) => (await p.waitForTimeout(1500), (await report(p)).peers.find((q) => q.id === id)?.theirHosts);
  const gctx = await browsers[0].newContext({ viewport: { width: 1360, height: 900 } });
  const hctx = await browsers[0].newContext({ viewport: { width: 1360, height: 900 } });
  await gctx.grantPermissions(['microphone'], { origin });
  const [gSettled, hSettled] = await Promise.allSettled([
    (async () => {
      const [g1, g2] = [await open(gctx, globalUrl('golf')), await open(gctx, globalUrl('hotel'))];
      const ids = await Promise.all([g1, g2].map((p) => wait(p, () => window.netlab.self).then(() => p.evaluate(() => window.netlab.self))));
      await Promise.all([linkedTo(g1, ids[1]), linkedTo(g2, ids[0])]);
      const r1 = await report(g1);
      return { hosts: [await hostsOf(g1, ids[1]), await hostsOf(g2, ids[0])], mic: r1.me.mediaPermission,
        joinLog: r1.log.find((e) => e.msg.startsWith('joining the global room'))?.msg, note: await g1.textContent('#micmsg'), page: g1 };
    })(),
    (async () => {
      const [h1, h2] = [await open(hctx, globalUrl('india')), await open(hctx, globalUrl('juliet'))];
      const old = await Promise.all([h1, h2].map((p) => wait(p, () => window.netlab.self).then(() => p.evaluate(() => window.netlab.self))));
      await Promise.all([linkedTo(h1, old[1]), linkedTo(h2, old[0])]);
      await hctx.grantPermissions(['microphone'], { origin });
      const ids = await Promise.all([h1, h2].map((p, i) => wait(p, (o) => window.netlab.self && window.netlab.self !== o, old[i], 20000).then(() => p.evaluate(() => window.netlab.self))));
      await Promise.all([linkedTo(h1, ids[1]), linkedTo(h2, ids[0])]);
      return { hosts: [await hostsOf(h1, ids[1]), await hostsOf(h2, ids[0])], rejoinLog: (await report(h1)).log.some((e) => e.msg.startsWith('the microphone is allowed on this site: rejoining the global room')) };
    })(),
  ]);
  const none = (hosts) => hosts.every((h) => Array.isArray(h) && !h.length);
  if (gSettled.status === 'rejected') ok(false, `global room, grant stored before: ${gSettled.reason.message}`);
  else {
    const g = gSettled.value;
    ok(g.mic === 'granted' && none(g.hosts), `global room, grant stored before: linked, local addresses given ${JSON.stringify(g.hosts)}`);
    ok(/without local addresses/.test(g.joinLog) && /kept out of the global room/.test(g.note), `and it says so: "${g.joinLog}" · "${g.note.split(' · ')[1]}"`);
    if (shot) await card(g.page).screenshot({ path: `${shot}-global.png` });
  }
  if (hSettled.status === 'rejected') ok(false, `global room, granted while in it: ${hSettled.reason.message}`);
  else ok(hSettled.value.rejoinLog && none(hSettled.value.hosts), `global room, granted while in it: rejoined, linked, local addresses given ${JSON.stringify(hSettled.value.hosts)}`);

  // A refused prompt: 'denied', a warning, still the same member of the room.
  const dctx = await launch('--use-fake-ui-for-media-stream=deny');
  const d = await open(dctx, `${origin}/demo/netlab.html?signal=local&name=delta#room=mic-deny-${port}`);
  await wait(d, () => window.netlab.self && window.netlab.report().me.network);
  const self = await d.evaluate(() => window.netlab.self);
  await d.click('#mic');
  await wait(d, () => window.netlab.report().me.mediaPermission !== 'prompt', null, 10000).catch(() => {});
  await d.waitForTimeout(1000);
  const denied = await report(d);
  ok(denied.me.mediaPermission === 'denied', `a refused prompt: mediaPermission ${denied.me.mediaPermission}`);
  ok(denied.log.some((e) => e.level === 'warn' && e.msg.startsWith('microphone not allowed')), `a warning: ${denied.log.find((e) => e.msg.startsWith('microphone'))?.msg}`);
  ok(denied.me.id === self && !(await d.isDisabled('#mic')), 'not rejoined, and the button can be pressed again');
  if (shot) await card(d).screenshot({ path: `${shot}-denied.png` });
} catch (e) {
  ok(false, `error: ${e.message}`);
} finally {
  for (const b of browsers) await b.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
