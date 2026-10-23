// Net Lab: a birthday punch that works carries the room link (room.adoptChannel). Two
// tabs in headless Chromium on loopback whose room WebRTC fails (?rtc=off), so they
// link over the signalling channel (BroadcastChannel). A real DataChannel is then made
// between the two tabs (loopback, outside the room) and handed to each tab as a punch's
// winning channel (netlab.punchWon: a real punch can't meet on one machine's one NAT).
// Checks that both tabs move the room link onto it (route direct, log lines with the
// channel's address), that chat goes over it, and that closing it puts the link back
// on signalling with the tabs still linked.
//
//   node packages/js/test/netlab_carry.mjs [screenshot-prefix]
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
const pages = [];
const errors = [];
const logHas = (page, re, timeout = 15000) => page.waitForFunction((src) => [...document.querySelectorAll('#log div')].some((d) => new RegExp(src).test(d.textContent)), re.source, { timeout });
try {
  await new Promise((r) => setTimeout(r, 600));
  // Both tabs' log ticks must run while the other has focus (as arena.mjs): a throttled
  // background tab can miss the gate's timeouts on a loaded host.
  browser = await chromium.launch({
    args: [
      '--disable-features=WebRtcHideLocalIpsWithMdns',
      '--disable-background-timer-throttling',
      '--disable-renderer-backgrounding',
      '--disable-backgrounding-occluded-windows',
    ],
  });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
  const key = `carry${port}abcdefgh`;
  for (let i = 0; i < 2; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push(`tab ${i}: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/demo/netlab.html?signal=local&rtc=off&name=c${i}#key=${key}`);
    pages.push(p);
  }
  const all = (fn, arg, timeout = 40000) => Promise.all(pages.map((p) => p.waitForFunction(fn, arg, { timeout })));
  await all(() => window.netlab?.peers().length === 1 && window.netlab.peers()[0].route === 'signaling');
  ok(true, 'two tabs linked over signalling (their room WebRTC fails)');

  // A real DataChannel between the tabs, outside the room: the stand-in for a punch's winner.
  const open = (sdp) => {
    window.chan = { pc: new RTCPeerConnection() };
    window.chan.dc = window.chan.pc.createDataChannel('peerwire', { negotiated: true, id: 0, ordered: false, maxRetransmits: 0 });
    const { pc } = window.chan;
    const gathered = () => new Promise((r) => (pc.iceGatheringState === 'complete' ? r() : pc.addEventListener('icegatheringstatechange', () => pc.iceGatheringState === 'complete' && r())));
    return (async () => {
      if (sdp) await pc.setRemoteDescription({ type: 'offer', sdp });
      await pc.setLocalDescription(await (sdp ? pc.createAnswer() : pc.createOffer()));
      await gathered();
      return pc.localDescription.sdp;
    })();
  };
  const offer = await pages[0].evaluate(open, null);
  const answer = await pages[1].evaluate(open, offer);
  await pages[0].evaluate((sdp) => window.chan.pc.setRemoteDescription({ type: 'answer', sdp }), answer);
  await all(() => window.chan.dc.readyState === 'open', null, 10000);
  ok(true, 'a direct loopback channel between the tabs is open');

  // Both tabs get it as a punch's winning channel, at about the same time (as both sides of a punch do).
  await Promise.all(pages.map((p) => p.evaluate(() => { window.netlab.punchWon(0, window.chan); })));
  await all(() => window.netlab.peers()[0].route === 'direct', null, 10000);
  ok(true, 'both tabs moved the room link onto the channel (route direct)');
  await Promise.all(pages.map((p) => logHas(p, /room link to \S+ now goes over the birthday-punch channel \(was over the signalling relays\)/)));
  ok(true, 'both logs say the room link now goes over the birthday-punch channel');
  await Promise.all(pages.map((p) => logHas(p, /route to \S+: direct to \d+\.\d+\.\d+\.\d+:\d+ \(IPv4 UDP, your .+ ↔ their .+\), over the birthday-punch channel/)));
  ok(true, "both logs give the channel's address and pair");
  ok((await pages[0].evaluate(() => window.netlab.birthday()[0].carried)) === true, 'the report has carried: true');

  // Chat goes over it.
  await pages[0].evaluate(() => window.netlab.say('over the punched channel'));
  await pages[1].waitForFunction(() => window.netlab.chat().some((c) => c.endsWith('over the punched channel')), null, { timeout: 10000 });
  ok(true, 'chat arrives over the channel');
  if (shot) await pages[0].screenshot({ path: `${shot}-carry.png`, fullPage: true });

  // The route ticks (every 250 ms) log the move; wait for them, so the close is a change they see.
  await Promise.all(pages.map((p) => logHas(p, /(→|reached) direct \(birthday-punch channel\)/)));
  ok(true, "both tabs' route logs show the move to the birthday-punch channel");

  // The channel closes: back on signalling, still linked, and chat still works.
  await pages[0].evaluate(() => window.chan.pc.close());
  await all(() => window.netlab.peers()[0]?.route === 'signaling', null, 10000);
  await Promise.all(pages.map((p) => logHas(p, /route to \S+: direct \(birthday-punch channel\) → over the signalling relays/)));
  ok(true, 'the channel closed: both tabs logged the link back on signalling');
  await pages[1].evaluate(() => window.netlab.say('back on signalling'));
  await pages[0].waitForFunction(() => window.netlab.chat().some((c) => c.endsWith('back on signalling')), null, { timeout: 15000 });
  ok(await pages[0].evaluate(() => window.netlab.peers().length === 1), 'still linked, and chat arrives over signalling again');

  if (errors.length) throw new Error(`page errors: ${errors.join('; ')}`);
} catch (e) {
  ok(false, e.stack ?? String(e));
  if (errors.length) console.log(`page errors: ${errors.join('; ')}`);
  // Each tab's route and channel log lines, newest first, to see what it did.
  for (const [i, p] of pages.entries()) {
    const lines = await p.evaluate(() => [...document.querySelectorAll('#log div')].map((d) => d.textContent).filter((t) => /route|channel|reached|error|ERROR/.test(t)).slice(0, 8)).catch(() => []);
    console.log(`tab ${i} log:\n  ${lines.join('\n  ')}`);
  }
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
