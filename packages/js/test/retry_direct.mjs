// Room retries of WebRTC (Room.retryDirect, src/retry.js) in headless Chromium, two tabs
// of one page on loopback with BroadcastChannel signalling:
// 1. each tab's first RTCPeerConnection never sends or takes a candidate (as Net Lab's
//    ?rtc=off), so the pair links over signalling; the backoff's retry, a real new
//    connection, opens and the link moves onto it: both routes 'direct', one session
//    (no second 'connected', no peer-leave or second peer-join), chat both ways after;
// 2. MIC-1: with the microphone granted (so new connections see the real local
//    addresses) and privacy 'public-only', no signal either tab sends, retries
//    included, names a host candidate or a local address; the retry fails quietly.
//
//   node packages/js/test/retry_direct.mjs
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const origin = `http://127.0.0.1:${port}`;
const browsers = [];
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};

// In a tab: joins `roomId`, keeping counts in window.T. `dead`: how many of this tab's
// first connections never send or take a candidate. `record`: signalling that keeps
// every message this tab sends (no trickle then, as over Nostr).
async function join({ roomId, name, dead, privacy, record }) {
  const { joinRoom } = await import('/src/index.js');
  const { DirectRetries } = await import('/src/retry.js');
  const { BroadcastSignaling } = await import('/src/webrtc.js');
  DirectRetries.firstMs = 1500;
  DirectRetries.gapMs = 200;
  const strip = (sdp) => sdp.split('\r\n').filter((l) => !l.startsWith('a=candidate:')).join('\r\n');
  let made = 0;
  class FailFirst extends RTCPeerConnection {
    constructor(cfg) {
      super(cfg);
      this.dead = ++made <= dead;
    }
    set onicecandidate(fn) {
      if (!this.dead) super.onicecandidate = fn;
    }
    get onicecandidate() {
      return super.onicecandidate;
    }
    get localDescription() {
      const d = super.localDescription;
      if (!d || !this.dead) return d;
      return { type: d.type, sdp: strip(d.sdp), toJSON: () => ({ type: d.type, sdp: strip(d.sdp) }) };
    }
    setRemoteDescription(d) {
      return super.setRemoteDescription(this.dead ? { type: d.type, sdp: strip(d.sdp) } : d);
    }
    addIceCandidate(c) {
      return this.dead ? Promise.resolve() : super.addIceCandidate(c);
    }
  }
  const sent = [];
  const signaling = record ? ({ endpoint, room }) => {
    const s = new BroadcastSignaling(endpoint.id, `peerwire/${room}`);
    const send = s.send.bind(s);
    s.send = (to, body) => (sent.push(JSON.stringify(body)), send(to, body));
    return s;
  } : 'local';
  const room = await joinRoom({ appId: 'peerwire-test', roomId, presence: { name }, signaling, iceServers: [], privacy, fallbackMs: 2000, RTCPeerConnection: FailFirst });
  const T = { room, sent, connected: 0, joins: 0, leaves: 0, chat: [], routes: [] };
  const ev = room.endpoint.onEvent;
  room.endpoint.onEvent = (e) => {
    if (e.type === 'connected') T.connected++;
    ev(e);
  };
  room.on('peer-join', () => T.joins++);
  room.on('peer-leave', () => T.leaves++);
  room.on('message', (m) => T.chat.push(m));
  // Each route the peer had, in order.
  setInterval(() => {
    const r = room.peers.map((p) => room.route(p)?.kind).join();
    if (r && T.routes.at(-1) !== r) T.routes.push(r);
  }, 50);
  window.T = T;
  return room.self;
}
const state = (p) => p.evaluate(() => ({ routes: window.T.routes, route: window.T.room.peers.map((q) => window.T.room.route(q)?.kind).join(), connected: window.T.connected,
  joins: window.T.joins, leaves: window.T.leaves, chat: window.T.chat, retried: window.T.room.transport.retried, peers: window.T.room.peers.length }));

try {
  await new Promise((r) => setTimeout(r, 600));
  const wait = (p, fn, arg, timeout = 30000) => p.waitForFunction(fn, arg, { timeout });

  // 1. First connections dead: over signalling, then the retry goes direct.
  {
    const b = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
    browsers.push(b);
    const ctx = await b.newContext();
    const pages = [await ctx.newPage(), await ctx.newPage()];
    for (const p of pages) {
      p.on('pageerror', (e) => ok(false, `page error: ${e.stack ?? e.message}`));
      await p.goto(`${origin}/`);
    }
    const roomId = `retry-${port}`;
    const ids = await Promise.all(pages.map((p, i) => p.evaluate(join, { roomId, name: `t${i}`, dead: 1, privacy: 'direct' })));
    await Promise.all(pages.map((p) => wait(p, () => window.T.room.peers.length === 1)));
    const t0 = Date.now();
    await Promise.all(pages.map((p) => wait(p, () => window.T.room.peers.map((q) => window.T.room.route(q)?.kind).join() === 'direct')));
    const took = Date.now() - t0;
    for (const p of pages) await p.evaluate(() => window.T.room.send(`hello from ${window.T.room.self.slice(0, 6)}`));
    await Promise.all(pages.map((p) => wait(p, () => window.T.chat.length === 1)));
    const [s0, s1] = await Promise.all(pages.map(state));
    const dialer = ids[0] < ids[1] ? s0 : s1;
    ok(s0.routes[0] === 'signaling' && s1.routes[0] === 'signaling', `first linked over signalling (first routes ${s0.routes[0]}, ${s1.routes[0]})`);
    ok(s0.route === 'direct' && s1.route === 'direct', `then direct after the retry, ${took} ms after linking`);
    ok(dialer.retried === 1 && s0.retried + s1.retried === 1, `one retry, by the dialling side (${s0.retried}, ${s1.retried})`);
    ok([s0, s1].every((s) => s.connected === 1 && s.joins === 1 && !s.leaves), `one session, no leave (${JSON.stringify([s0, s1].map(({ connected, joins, leaves }) => ({ connected, joins, leaves })))})`);
    ok(s0.chat[0]?.startsWith('hello from') && s1.chat[0]?.startsWith('hello from'), 'chat both ways after it');
    const pair = await pages[0].evaluate(async () => {
      const t = window.T.room.transport;
      const l = t.links.get(t.byPeer.get(window.T.room.peers[0]));
      const stats = await l.pc.getStats();
      for (const s of stats.values()) if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') return { pc: l.pc.connectionState, dc: l.dc.readyState };
      return { pc: l.pc.connectionState, dc: l.dc.readyState, none: true };
    });
    ok(pair.pc === 'connected' && pair.dc === 'open' && !pair.none, `the link runs on the retry's connection (the first could not connect): ${JSON.stringify(pair)}`);
    await b.close();
  }

  // 2. MIC-1: microphone granted, 'public-only': a retry names no local address.
  {
    const b = await chromium.launch({ args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'] });
    browsers.push(b);
    const ctx = await b.newContext();
    await ctx.grantPermissions(['microphone'], { origin });
    const pages = [await ctx.newPage(), await ctx.newPage()];
    for (const p of pages) {
      p.on('pageerror', (e) => ok(false, `page error: ${e.stack ?? e.message}`));
      await p.goto(`${origin}/`);
    }
    // The grant shows the real local addresses to new connections: list them.
    const locals = await pages[0].evaluate(async () => {
      (await navigator.mediaDevices.getUserMedia({ audio: true })).getTracks().forEach((t) => t.stop());
      const pc = new RTCPeerConnection();
      pc.createDataChannel('x');
      const out = new Set();
      pc.onicecandidate = (e) => {
        const m = / (\S+) \d+ typ host/.exec(e.candidate?.candidate ?? '');
        if (m) out.add(m[1]);
      };
      await pc.setLocalDescription(await pc.createOffer());
      await new Promise((r) => setTimeout(r, 1500));
      pc.close();
      return [...out];
    });
    ok(locals.length > 0 && !locals.some((a) => a.endsWith('.local')), `after the grant, new connections see local addresses: ${locals.join(', ')}`);
    const roomId = `retry-mic-${port}`;
    await Promise.all(pages.map((p, i) => p.evaluate(join, { roomId, name: `m${i}`, dead: 0, privacy: 'public-only', record: true })));
    await Promise.all(pages.map((p) => wait(p, () => window.T.room.peers.length === 1)));
    // Linked over signalling (no STUN here, so no address to offer): wait for a retry
    // and its failure, then read what each tab sent.
    await Promise.all(pages.map((p) => wait(p, () => window.T.sent.some((s) => s.includes('"retry":true')) || window.T.room.transport.retried === 0)));
    await pages[0].waitForTimeout(3000);
    const [s0, s1] = await Promise.all(pages.map(state));
    const sent = (await Promise.all(pages.map((p) => p.evaluate(() => window.T.sent)))).flat();
    const retryOffers = sent.filter((s) => s.includes('"kind":"offer"') && s.includes('"retry":true'));
    const leaks = sent.filter((s) => / typ host/.test(s) || locals.some((a) => s.includes(a)));
    ok(s0.retried + s1.retried >= 1 && retryOffers.length >= 1, `a retry offered (${retryOffers.length} retry offers, ${sent.length} signals)`);
    ok(!leaks.length, `no signal names a host candidate or a local address${leaks.length ? `: ${leaks[0].slice(0, 300)}` : ''}`);
    ok(s0.route === 'signaling' && s1.route === 'signaling' && !s0.leaves && !s1.leaves && s0.connected === 1, `the retry failed quietly: still over signalling, no leave (${s0.route}, ${s1.route})`);
    await b.close();
  }
} catch (e) {
  ok(false, e.stack ?? e.message);
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  server.kill();
}
console.log(failed ? `retry direct: ${failed} failed` : 'retry direct: all passed');
process.exit(failed ? 1 : 0);
