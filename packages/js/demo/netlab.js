// Net Lab: a connection diagnostics page on Peerwire rooms, not a game. Join the
// global room (everyone using Net Lab meets there) or a private room (a random
// key, shared as a link), and every member links to every other one. For each
// peer it shows the route (direct WebRTC, relayed by a member, or over the
// signalling relays), how the WebRTC link was made (ICE candidate pair, IPv4 or
// IPv6, UDP or TCP), the time it took, and a ping measured by the page itself on
// whatever route is in use (min/avg/max, jitter, loss). It also probes this
// device's network (public addresses, IPv6, a NAT mapping estimate), shows the
// signalling relays' state, has a chat, logs every event, warning and error, and
// copies all of it as one JSON report.
//
//   ?signal=local      BroadcastChannel signalling (tabs of one browser; tests)
//   ?rtc=off           fail every WebRTC link of this tab, as behind a symmetric NAT
//   ?relays=wss://a,…  other Nostr relays
//   ?name=…            the name; ?auto=global joins the global room
//   #key=…             joins that private room (a hash, so it never reaches the server)

import { joinRoom, randomRoomId, DEFAULT_RELAYS } from '../src/index.js';

const APP_ID = 'peerwire-netlab';
const GLOBAL_ROOM = 'global-v1';
// Two STUN servers: the same public port from both means the NAT maps one port per
// socket (cone-like); different ports mean a new mapping per destination (symmetric).
const PROBE_STUN = [{ urls: 'stun:stun.l.google.com:19302' }, { urls: 'stun:stun.cloudflare.com:3478' }];
const PING_DIRECT_MS = 1000;
const PING_FALLBACK_MS = 3000; // the signalling route carries a few batches a second in all
const PING_LOST_MS = 10000;
const SAMPLES = 60;
const HEARD_WARN_MS = 20000;
const SLOW_MS = 400;

const params = new URLSearchParams(location.search);
const $ = (id) => document.getElementById(id);
const now = () => performance.now();
const short = (id) => String(id).slice(0, 6);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const ms = (v) => (v == null || !Number.isFinite(v) ? '–' : `${Math.round(v)} ms`);
const secs = (v) => (v == null ? '–' : `${(v / 1000).toFixed(1)} s`);
const clip = (s, n) => String(s ?? '').slice(0, n);
const t0 = Date.now();

let room = null;
let roomKind = null; // 'global' | 'private'
let roomKey = null;
let joinedAt = 0;
let myNet = null; // the result of probeNetwork()
const peers = new Map(); // peer id -> P (see peerState)
const chat = [];
const events = [];
let warnings = 0;
const relaySeen = new Map(); // relay url -> last state logged
let lastRejected = 0;
let lastBackoffs = 0;

// ---------- the event log ----------

function log(level, msg) {
  const at = Date.now();
  events.push({ at, level, msg });
  if (events.length > 500) events.shift();
  if (level !== 'info') warnings++;
  const div = document.createElement('div');
  div.className = level;
  div.textContent = `${new Date(at).toLocaleTimeString()} ${level === 'info' ? '' : `${level.toUpperCase()} `}${msg}`;
  $('log').prepend(div);
  while ($('log').children.length > 300) $('log').lastChild.remove();
  $('h-warn').textContent = `${warnings} warning${warnings === 1 ? '' : 's'}`;
  $('h-warn').className = `pill ${warnings ? 'warn' : ''}`;
}
window.addEventListener('error', (e) => log('error', `page error: ${e.message}`));
window.addEventListener('unhandledrejection', (e) => log('error', `unhandled: ${e.reason?.message ?? e.reason}`));
for (const level of ['warn', 'error']) {
  const orig = console[level].bind(console);
  console[level] = (...a) => { orig(...a); log(level, `console: ${a.map((x) => (x instanceof Error ? x.message : typeof x === 'string' ? x : JSON.stringify(x))).join(' ').slice(0, 300)}`); };
}
addEventListener('online', () => log('info', 'browser is online'));
addEventListener('offline', () => log('warn', 'browser went offline'));
navigator.connection?.addEventListener?.('change', () => log('info', `network changed: ${connInfo()}`));

// ---------- this device ----------

function device() {
  const ua = navigator.userAgent;
  const os = /Android/.test(ua) ? 'Android' : /iPhone|iPad|iPod/.test(ua) ? 'iOS' : /Windows/.test(ua) ? 'Windows' : /Mac OS X/.test(ua) ? 'macOS' : /CrOS/.test(ua) ? 'ChromeOS' : /Linux/.test(ua) ? 'Linux' : 'other';
  const br = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /OPR\//.test(ua) ? 'Opera' : /SamsungBrowser/.test(ua) ? 'Samsung' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'other';
  return `${br} on ${os}${/Mobi|Android|iPhone/.test(ua) ? ' (mobile)' : ''}`;
}
function connInfo() {
  const c = navigator.connection;
  if (!c) return 'unknown';
  return [c.type, c.effectiveType && `~${c.effectiveType}`, c.rtt != null && `rtt≈${c.rtt}ms`, c.downlink != null && `${c.downlink}Mb/s`].filter(Boolean).join(', ');
}

// Gathers ICE candidates against two STUN servers: public addresses, IPv6, and
// whether the NAT keeps one public port for both (a NAT mapping estimate).
async function probeNetwork() {
  $('mynet').innerHTML = '<span class="dim">testing (up to 8 s)…</span>';
  const started = now();
  const res = { at: new Date().toISOString(), host: [], srflx: [], firstStunMs: null, nat: 'unknown', ipv6: false, udp: false, notes: [] };
  let pc;
  try {
    pc = new RTCPeerConnection({ iceServers: PROBE_STUN });
  } catch (e) {
    res.notes.push(`WebRTC unavailable: ${e.message}`);
    log('error', `WebRTC unavailable: ${e.message}`);
    return res;
  }
  pc.createDataChannel('probe');
  await new Promise(async (done) => {
    const timer = setTimeout(done, 8000);
    pc.onicegatheringstatechange = () => { if (pc.iceGatheringState === 'complete') { clearTimeout(timer); done(); } };
    pc.onicecandidate = (e) => {
      const c = e.candidate;
      if (!c) return;
      const entry = { address: c.address ?? '', port: c.port, protocol: c.protocol, server: e.url ?? null };
      if (c.type === 'srflx') {
        res.srflx.push(entry);
        // Gathering can take many seconds to 'complete' (unreachable IPv6, slow servers):
        // the second server's mapping comes within moments of the first, if at all.
        if (res.firstStunMs == null) setTimeout(() => { clearTimeout(timer); done(); }, 2500);
        res.firstStunMs ??= Math.round(now() - started);
      } else if (c.type === 'host') res.host.push(entry);
    };
    try {
      await pc.setLocalDescription(await pc.createOffer());
    } catch (e) {
      res.notes.push(`offer failed: ${e.message}`);
      clearTimeout(timer);
      done();
    }
  });
  pc.close();
  const v6 = (a) => a.includes(':');
  res.udp = res.srflx.some((c) => c.protocol === 'udp');
  res.ipv6 = res.srflx.some((c) => v6(c.address)) || res.host.some((c) => v6(c.address) && !c.address.endsWith('.local'));
  // Per public IPv4 address: how many public ports one socket got from the two servers.
  const byIp = new Map();
  for (const c of res.srflx.filter((c) => !v6(c.address))) byIp.set(c.address, new Set([...(byIp.get(c.address) ?? []), c.port]));
  if (!res.srflx.length) {
    res.nat = 'no STUN answer';
    res.notes.push('No STUN server answered: UDP to the internet may be blocked, so direct WebRTC will fail; only the fallback over the signalling relays (WebSocket) can work.');
  } else if ([...byIp.values()].some((ports) => ports.size > 1)) {
    res.nat = 'symmetric';
    res.notes.push('The public port changes per destination (symmetric NAT, typical of mobile carriers/CGNAT). Direct WebRTC to another symmetric NAT will fail; expect a relayed or signalling route for those peers.');
  } else if (byIp.size) {
    res.nat = 'cone-like';
    res.notes.push('One public port for both STUN servers (endpoint-independent mapping): direct links should work unless the other side is very strict. (If one STUN server did not answer, this can be a false good result.)');
  } else {
    res.nat = 'IPv6 only';
  }
  if (res.ipv6) res.notes.push('IPv6 works: two peers that both have IPv6 can usually link directly with no NAT in the way.');
  res.durationMs = Math.round(now() - started);
  log(res.srflx.length ? 'info' : 'warn', `network test: NAT ${res.nat}, IPv6 ${res.ipv6 ? 'yes' : 'no'}, ${res.srflx.length} public candidate(s), first STUN answer ${ms(res.firstStunMs)}`);
  return res;
}

function renderMyNet() {
  if (!myNet) return;
  const pub = [...new Set(myNet.srflx.map((c) => `${c.address.includes(':') ? `[${c.address}]` : c.address}:${c.port}`))];
  const natCls = myNet.nat === 'cone-like' || myNet.nat === 'IPv6 only' ? 'ok' : myNet.nat === 'symmetric' ? 'warn' : 'bad';
  $('mynet').innerHTML = `<dl>
    <dt>Device</dt><dd>${esc(device())}</dd>
    <dt>Connection</dt><dd>${esc(connInfo())}</dd>
    <dt>NAT</dt><dd class="${natCls}">${esc(myNet.nat)}</dd>
    <dt>IPv6</dt><dd class="${myNet.ipv6 ? 'ok' : 'dim'}">${myNet.ipv6 ? 'yes' : 'no'}</dd>
    <dt>Public</dt><dd class="mono">${pub.map(esc).join('<br>') || '–'}</dd>
    <dt>Local</dt><dd class="mono">${[...new Set(myNet.host.map((c) => c.address))].map(esc).join(', ') || '–'}</dd>
    <dt>STUN</dt><dd>first answer ${ms(myNet.firstStunMs)}, test took ${ms(myNet.durationMs)}</dd>
  </dl>${myNet.notes.map((n) => `<div class="note dim">${esc(n)}</div>`).join('')}`;
}

async function reprobe() {
  $('reprobe').disabled = true;
  myNet = await probeNetwork();
  renderMyNet();
  $('reprobe').disabled = false;
  if (room) room.setPresence(presence());
}

function presence() {
  return { name: myName(), dev: device(), conn: connInfo(), nat: myNet?.nat ?? 'testing', v6: !!myNet?.ipv6 };
}
const myName = () => clip($('name').value.trim(), 20) || `guest-${short(room?.self ?? '000000')}`;

// ---------- joining ----------

// An RTCPeerConnection that never gathers or accepts a candidate, so ICE fails (?rtc=off).
class NoIcePc extends RTCPeerConnection {
  set onicecandidate(fn) {}
  get localDescription() {
    const d = super.localDescription;
    return d && { type: d.type, sdp: strip(d.sdp), toJSON: () => ({ type: d.type, sdp: strip(d.sdp) }) };
  }
  setRemoteDescription(d) { return super.setRemoteDescription({ type: d.type, sdp: strip(d.sdp) }); }
  addIceCandidate() { return Promise.resolve(); }
}
const strip = (sdp) => sdp.split('\r\n').filter((l) => !l.startsWith('a=candidate:')).join('\r\n');

function parseKey(s) {
  s = String(s ?? '').trim();
  const m = s.match(/[#&?]key=([A-Za-z0-9_-]+)/);
  return m ? m[1] : /^[A-Za-z0-9_-]{8,64}$/.test(s) ? s : null;
}

async function enter(kind, key) {
  if (room) return;
  for (const b of ['global', 'create', 'joinkey']) $(b).disabled = true;
  localStorage.setItem('netlab-name', $('name').value.trim());
  roomKind = kind;
  roomKey = kind === 'private' ? key : null;
  $('joinmsg').textContent = 'connecting to the signalling relays…';
  const relays = params.get('relays')?.split(',').filter(Boolean);
  const signaling = params.get('signal') === 'local' ? 'local' : 'nostr';
  log('info', `joining the ${kind} room over ${signaling === 'local' ? 'local BroadcastChannel' : `Nostr (${(relays ?? DEFAULT_RELAYS).join(', ')})`}${params.get('rtc') === 'off' ? ', WebRTC disabled (?rtc=off)' : ''}`);
  const started = now();
  try {
    room = await joinRoom({
      appId: APP_ID,
      roomId: kind === 'global' ? GLOBAL_ROOM : key,
      signaling,
      presence: { name: clip($('name').value.trim(), 20) || 'guest', dev: device(), conn: connInfo(), nat: myNet?.nat ?? 'testing', v6: !!myNet?.ipv6 },
      ...(relays?.length && { relays }),
      ...(params.get('rtc') === 'off' && { RTCPeerConnection: NoIcePc }),
    });
  } catch (e) {
    log('error', `could not join: ${e.message}`);
    $('joinmsg').textContent = `could not join: ${e.message}`;
    for (const b of ['global', 'create', 'joinkey']) $(b).disabled = false;
    return;
  }
  joinedAt = now();
  if (!$('name').value.trim()) room.setPresence(presence());
  log('info', `joined in ${ms(joinedAt - started)} as ${short(room.self)}; waiting for peers`);
  hook();
  if (kind === 'private') history.replaceState(null, '', `${location.pathname}${location.search}#key=${key}`);
  $('join').classList.add('hidden');
  $('roomcard').classList.remove('hidden');
  $('share').classList.toggle('hidden', kind !== 'private');
  $('chatsend').disabled = false;
  $('joinmsg').textContent = '';
  render();
}

function peerState(peer) {
  let p = peers.get(peer);
  if (!p) {
    p = { id: peer, heardAt: null, rtcOpenAt: null, rtcFailedAt: null, joinedAt: null, leftAt: null, leaveReason: null, presence: null,
      route: null, routeLog: [], ice: null, sig: [], pairs: new Map(), pings: new Map(), seq: 0, lastPing: 0, samples: [], sent: 0, recv: 0, lost: 0, warned: new Set() };
    peers.set(peer, p);
  }
  return p;
}
const nameOf = (peer) => clip(peers.get(peer)?.presence?.name ?? short(peer), 20);
function warnOnce(p, key, msg) {
  if (p.warned.has(key)) return;
  p.warned.add(key);
  log('warn', msg);
}

function hook() {
  const sig = room.signaling;
  const hello = sig.onHello;
  sig.onHello = (peer, meta) => {
    const p = peerState(peer);
    if (!p.heardAt || p.leftAt) {
      p.heardAt = now();
      p.leftAt = null;
      log('info', `heard ${short(peer)} through signalling`);
    }
    hello(peer, meta);
  };
  const t = room.transport;
  if (t && 'trace' in t) t.trace = onTrace;
  if (t?.onPeer) {
    const onPeer = t.onPeer;
    t.onPeer = (peer, state) => {
      const p = peerState(peer);
      if (state === 'open') {
        p.rtcOpenAt = now();
        log('info', `WebRTC link to ${nameOf(peer)} open${p.heardAt ? ` ${secs(p.rtcOpenAt - p.heardAt)} after hearing it` : ''}`);
      } else if (state === 'failed') {
        p.rtcFailedAt = now();
        log('warn', `WebRTC to ${nameOf(peer)} failed (ICE found no working pair): trying a member relay, then the signalling relays`);
      } else if (state === 'closed') log('info', `WebRTC link to ${nameOf(peer)} closed`);
      onPeer(peer, state);
    };
  }
  room.on('peer-join', (peer, pres) => {
    const p = peerState(peer);
    p.presence = pres;
    p.joinedAt = now();
    p.leftAt = null;
    log('info', `${nameOf(peer)} joined (${clip(pres?.dev, 40)}, NAT ${clip(pres?.nat, 20)}, IPv6 ${pres?.v6 ? 'yes' : 'no'})${p.heardAt ? `, ${secs(p.joinedAt - p.heardAt)} after hearing it` : ''}`);
  });
  room.on('presence', (peer, pres) => { peerState(peer).presence = pres; });
  room.on('peer-leave', (peer, reason) => {
    const p = peerState(peer);
    p.leftAt = now();
    p.leaveReason = reason;
    p.pings.clear();
    log(reason === 'left' || reason === 'closed' ? 'info' : 'warn', `${nameOf(peer)} left: ${reason}`);
  });
  room.on('message', onMessage);
}

// The addresses in an offer, answer or late batch, as "1 IPv6 public, 2 hidden (mDNS)".
function addrText(d) {
  const parts = [[d.host6, 'IPv6 host'], [d.host4, 'IPv4 host'], [d.srflx6, 'IPv6 public'], [d.srflx4, 'IPv4 public'], [d.mdns, 'hidden (mDNS)'], [d.relay, 'TURN']]
    .filter(([n]) => n).map(([n, k]) => `${n} ${k}`);
  return parts.length ? parts.join(', ') : 'no addresses';
}

const SIG_TEXT = {
  'offer-sent': (d) => d.resend ? 'no answer yet: offer sent again' : `offer sent (${addrText(d)})`,
  'offer-received': (d) => `offer received (${addrText(d)})`,
  'answer-sent': (d) => d.resend ? 'their offer came again (our answer lost?): answer sent again' : `answer sent (${addrText(d)})`,
  'answer-received': (d) => `answer received (${addrText(d)})`,
  'ice-batch-sent': (d) => `late addresses sent (${addrText(d)})`,
  'ice-batch-received': (d) => `late addresses received (${addrText(d)})`,
  'ice-state': (d) => `ICE ${d.state}`,
};

// The WebRTC transport's diagnostics for one link.
function onTrace(peer, what, d) {
  const p = peerState(peer);
  const at = p.heardAt ? Math.round(now() - p.heardAt) : null;
  if (what === 'candidate-error') {
    // Chrome reports one per local address and STUN server that failed; log each kind once.
    const fam = String(d.address ?? '').includes(':') ? 'IPv6' : 'IPv4';
    warnOnce(p, `cerr ${d.url} ${d.code} ${fam}`, `STUN ${clip(d.url, 60)} over ${fam}: error ${d.code} ${clip(d.text, 80)}`);
    return;
  }
  const text = SIG_TEXT[what]?.(d) ?? what;
  if (p.sig.length < 60) p.sig.push({ at, what, ...d });
  const bad = d.resend || (what === 'ice-state' && (d.state === 'disconnected' || d.state === 'failed'));
  log(bad ? 'warn' : 'info', `WebRTC with ${nameOf(peer)}: ${text}${at != null ? ` (${secs(at)} after hearing it)` : ''}`);
}

function onMessage(data, peer, info) {
  if (!data || typeof data !== 'object') return;
  const p = peerState(peer);
  if (data.t === 'ping' && Number.isFinite(data.s)) {
    room.send({ t: 'pong', s: data.s }, { to: peer, lane: info.lane });
  } else if (data.t === 'pong' && Number.isFinite(data.s)) {
    const sent = p.pings.get(data.s);
    if (sent == null) return;
    p.pings.delete(data.s);
    p.recv++;
    p.samples.push(now() - sent);
    if (p.samples.length > SAMPLES) p.samples.shift();
  } else if (data.t === 'chat' && typeof data.text === 'string') {
    addChat(nameOf(peer), clip(data.text, 500), routeLabel(room.route(peer)));
  }
}

// ---------- pinging ----------

function routeLabel(r) {
  if (!r) return 'not linked';
  if (r.kind === 'peer') return `relayed by ${nameOf(r.via)}`;
  if (r.kind === 'signaling') return 'over the signalling relays';
  return 'direct';
}

function tickPings() {
  if (!room) return;
  const t = now();
  for (const peer of room.peers) {
    const p = peerState(peer);
    const r = room.route(peer);
    const label = routeLabel(r);
    if (label !== p.route) {
      if (p.route) log(r?.kind === 'signaling' ? 'warn' : 'info', `route to ${nameOf(peer)}: ${p.route} → ${label}`);
      else if (r) log(r.kind === 'direct' ? 'info' : 'warn', `${nameOf(peer)} reached ${label}`);
      p.route = label;
      p.routeLog.push({ at: Math.round(t - joinedAt), route: label });
    }
    for (const [s, at] of p.pings) {
      if (t - at > PING_LOST_MS) {
        p.pings.delete(s);
        p.lost++;
      }
    }
    const every = r?.kind === 'direct' ? PING_DIRECT_MS : PING_FALLBACK_MS;
    if (t - p.lastPing >= every) {
      p.lastPing = t;
      const s = ++p.seq;
      try {
        room.send({ t: 'ping', s }, { to: peer, lane: 'unreliable' });
        p.pings.set(s, t);
        p.sent++;
      } catch (e) {
        warnOnce(p, 'pingfail', `could not ping ${nameOf(peer)}: ${e.message}`);
      }
    }
    const st = pingStats(p);
    if (st.avg > SLOW_MS && st.n >= 3) warnOnce(p, 'slow', `ping to ${nameOf(peer)} averages ${ms(st.avg)} (${label})`);
    if (st.loss > 20 && p.sent >= 10) warnOnce(p, 'loss', `${st.loss.toFixed(0)}% of pings to ${nameOf(peer)} lost (${label})`);
  }
  for (const p of peers.values()) {
    if (p.heardAt && !p.joinedAt && !p.leftAt && t - p.heardAt > HEARD_WARN_MS) {
      warnOnce(p, 'nolink', `heard ${short(p.id)} ${secs(t - p.heardAt)} ago but no route works yet`);
    }
  }
}

function pingStats(p) {
  const s = p.samples;
  if (!s.length) return { n: 0, last: null, avg: null, min: null, max: null, jitter: null, loss: p.sent ? (100 * p.lost) / Math.max(1, p.recv + p.lost) : 0 };
  const avg = s.reduce((a, b) => a + b, 0) / s.length;
  let j = 0;
  for (let i = 1; i < s.length; i++) j += Math.abs(s[i] - s[i - 1]);
  return { n: s.length, last: s[s.length - 1], avg, min: Math.min(...s), max: Math.max(...s), jitter: s.length > 1 ? j / (s.length - 1) : 0,
    loss: (100 * p.lost) / Math.max(1, p.recv + p.lost) };
}

// ---------- WebRTC details ----------

async function iceOf(peer) {
  const link = [...(room.transport?.links?.values() ?? [])].find((l) => l.peer === peer);
  if (!link?.pc) return null;
  const pc = link.pc;
  const out = { connection: pc.connectionState, ice: pc.iceConnectionState, gathering: pc.iceGatheringState, channel: link.dc?.readyState ?? null };
  try {
    const stats = await pc.getStats();
    let pair = null;
    let local = 0, remote = 0;
    const p = peerState(peer);
    stats.forEach((r) => {
      if (r.type === 'transport' && r.selectedCandidatePairId) pair = stats.get(r.selectedCandidatePairId);
      if (r.type === 'local-candidate') local++;
      if (r.type === 'remote-candidate') remote++;
      if (r.type === 'candidate-pair') notePair(p, r, stats);
    });
    if (!pair) stats.forEach((r) => { if (r.type === 'candidate-pair' && r.nominated && r.state === 'succeeded') pair = r; });
    out.candidates = { local, remote };
    if (pair) {
      const l = stats.get(pair.localCandidateId), r = stats.get(pair.remoteCandidateId);
      const addr = (c) => c?.address ?? c?.ip ?? '';
      out.pair = {
        local: l?.candidateType, remote: r?.candidateType, protocol: l?.protocol,
        family: String(addr(l)).includes(':') ? 'IPv6' : 'IPv4',
        remoteAddress: addr(r) ? `${addr(r)}:${r.port}` : null,
        iceRttMs: pair.currentRoundTripTime != null ? Math.round(pair.currentRoundTripTime * 1000) : null,
        bytesSent: pair.bytesSent, bytesReceived: pair.bytesReceived,
      };
    }
  } catch { /* the link closed under us */ }
  return out;
}

// Chrome deletes candidate pairs that never got an answer (after ~15 s), so the
// counts are kept here: the most each pair ever reached.
const MAX_PAIRS = 24;
function notePair(p, r, stats) {
  const l = stats.get(r.localCandidateId), rc = stats.get(r.remoteCandidateId);
  const addr = (c) => c?.address ?? c?.ip ?? '';
  const fam = (c) => (String(addr(c)).includes(':') ? 'v6' : 'v4');
  const key = `${l?.candidateType}/${fam(l)} ↔ ${rc?.candidateType}/${fam(rc)} ${addr(rc)}:${rc?.port ?? ''}`;
  let q = p.pairs.get(key);
  if (!q) {
    if (p.pairs.size >= MAX_PAIRS) return;
    q = { pair: key, protocol: l?.protocol, state: r.state, requestsSent: 0, responsesReceived: 0, requestsReceived: 0, responsesSent: 0 };
    p.pairs.set(key, q);
  }
  q.state = r.state;
  for (const k of ['requestsSent', 'responsesReceived', 'requestsReceived', 'responsesSent']) q[k] = Math.max(q[k], r[k] ?? 0);
}

// The offer/answer exchange, as "offer sent 0.2 s · answer received 1.9 s · 1 resend".
function sigText(p) {
  const first = (w) => p.sig.find((e) => e.what === w && !e.resend);
  const resends = p.sig.filter((e) => e.resend).length;
  const parts = [['offer-sent', 'offer sent'], ['offer-received', 'offer received'], ['answer-sent', 'answer sent'], ['answer-received', 'answer received'], ['ice-batch-received', 'late addresses received']]
    .map(([w, label]) => { const e = first(w); return e ? `${label}${e.at != null ? ` ${secs(e.at)}` : ''}` : null; }).filter(Boolean);
  const theirs = first('offer-received') ?? first('answer-received');
  // With trickle (local signalling) candidates come separately, not in the description.
  if (theirs && !room.transport?.trickle) parts.push(`their addresses: ${esc(addrText(theirs))}`);
  if (resends) parts.push(`<span class="warn">${resends} resent</span>`);
  if (first('offer-sent') && !first('answer-received') && !p.rtcOpenAt) parts.push('<span class="warn">no answer received</span>');
  return parts.join(' · ');
}

// What the connectivity checks say about the path between the two sides.
function checksText(p) {
  if (!p.pairs.size) return '';
  let sent = 0, replies = 0, inbound = 0, ok = false;
  for (const q of p.pairs.values()) {
    sent += q.requestsSent;
    replies += q.responsesReceived;
    inbound += q.requestsReceived;
    ok ||= q.state === 'succeeded';
  }
  let verdict = '';
  if (!ok && sent > 0) {
    if (inbound > 0 && !replies) verdict = 'their checks reach us, but ours get no reply: our packets do not reach them, or their replies do not reach us';
    else if (!inbound && !replies) verdict = 'no check got through in either direction: something between the two sides (a carrier or NAT filter) drops the packets';
    else if (replies) verdict = 'some checks answered, but no pair completed';
  }
  return `${p.pairs.size} pairs tried · ${sent} checks sent, ${replies} answered, ${inbound} received from them${verdict ? ` <span class="bad">· ${esc(verdict)}</span>` : ''}`;
}

const KIND = { host: 'LAN', srflx: 'public (STUN)', prflx: 'public (peer-seen)', relay: 'TURN' };
function iceText(ice) {
  if (!ice) return '<span class="dim">no WebRTC link</span>';
  if (!ice.pair) return `<span class="dim">ICE ${esc(ice.ice)}, ${ice.candidates?.local ?? 0} local / ${ice.candidates?.remote ?? 0} remote candidates</span>`;
  const p = ice.pair;
  return `${esc(p.family)} ${esc(String(p.protocol ?? '').toUpperCase())}, ${esc(KIND[p.local] ?? p.local)} ↔ ${esc(KIND[p.remote] ?? p.remote)}`
    + `${p.remoteAddress ? ` <span class="mono dim">${esc(p.remoteAddress)}</span>` : ''}`;
}

// What the two NAT results mean for a direct link.
function pairHint(pres) {
  if (!myNet || !pres) return '';
  if (myNet.ipv6 && pres.v6) return 'Both have IPv6: a direct IPv6 link should work, unless a carrier or firewall drops packets between the two (see ICE checks).';
  const a = myNet.nat, b = pres.nat;
  if (a === 'symmetric' && b === 'symmetric') return 'Both behind symmetric NAT and not both on IPv6: direct WebRTC cannot work without TURN; a member relay or the slow signalling route is expected.';
  if (a === 'symmetric' || b === 'symmetric') return 'One side is behind symmetric NAT: direct works only if the other side is open enough (cone NAT).';
  if (a === 'no STUN answer' || b === 'no STUN answer') return 'One side gets no STUN answer (UDP blocked?): only the signalling route can work.';
  return '';
}

// ---------- rendering ----------

function spark(samples) {
  if (samples.length < 2) return '';
  const w = 300, h = 34, max = Math.max(...samples, 50);
  const pts = samples.map((v, i) => `${((i / (SAMPLES - 1)) * w).toFixed(1)},${(h - 2 - (v / max) * (h - 4)).toFixed(1)}`).join(' ');
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polyline fill="none" stroke="#3e63dd" stroke-width="1.5" points="${pts}"/></svg>`;
}
const pingCls = (v) => (v == null ? 'dim' : v < 150 ? 'ok' : v < SLOW_MS ? 'warn' : 'bad');

let rendering = false;
async function render() {
  if (!room || rendering) return;
  rendering = true;
  try {
    const t = now();
    const linked = room.peers;
    const avgs = linked.map((id) => pingStats(peerState(id)).avg).filter((v) => v != null);
    const roomAvg = avgs.length ? avgs.reduce((a, b) => a + b, 0) / avgs.length : null;
    const routes = { direct: 0, peer: 0, signaling: 0 };
    for (const id of linked) { const k = room.route(id)?.kind; if (k) routes[k]++; }
    $('h-room').textContent = roomKind === 'global' ? 'global room' : `private ${roomKey.slice(0, 4)}…`;
    $('h-peers').textContent = `${linked.length} linked`;
    $('h-ping').textContent = `ping ${ms(roomAvg)}`;
    $('h-ping').className = `pill ${pingCls(roomAvg)}`;

    const heardOnly = [...peers.values()].filter((p) => p.heardAt && !p.joinedAt && !p.leftAt).length;
    const core = room.stats();
    $('roomstats').innerHTML = [
      ['room', roomKind === 'global' ? 'global (public)' : 'private'],
      ['me', `${esc(myName())} <span class="mono dim">${short(room.self)}</span>`],
      ['in room for', secs(t - joinedAt)],
      ['linked / waiting', `${linked.length} / ${heardOnly}`],
      ['routes', `${routes.direct} direct, ${routes.peer} relayed, ${routes.signaling} signalling`],
      ['room ping avg', `<span class="${pingCls(roomAvg)}">${ms(roomAvg)}</span>`],
      ['worst ping', ms(avgs.length ? Math.max(...avgs) : null)],
      ['packets', `${core.txPackets} out / ${core.rxPackets} in, ${core.retransmits} resent`],
    ].map(([k, v]) => `<div><span>${k}</span>${v}</div>`).join('');

    const sig = room.signaling;
    let relayHtml = '';
    if (sig.sockets) {
      const st = (ws) => {
        const paused = sig.paused?.get(ws.relayUrl);
        if (paused?.until > Date.now()) return `rate-limited, paused ${secs(paused.until - Date.now())}`;
        return ['connecting', 'open', 'closing', 'closed'][ws.readyState];
      };
      for (const ws of sig.sockets) {
        const s = st(ws);
        const was = relaySeen.get(ws.relayUrl);
        if (s !== was && !(s?.startsWith('rate-limited') && was?.startsWith('rate-limited'))) {
          relaySeen.set(ws.relayUrl, s);
          log(s === 'open' || s === 'connecting' ? 'info' : 'warn', `signalling relay ${ws.relayUrl}: ${s}`);
        }
      }
      const s = sig.stats;
      if (s.rejected > lastRejected) log('warn', `a relay refused ${s.rejected - lastRejected} event(s): ${clip(s.lastRejection, 160)}`);
      if ((s.backoffs ?? 0) > lastBackoffs) log('warn', 'a signalling relay rate-limited us: pausing it');
      lastRejected = s.rejected;
      lastBackoffs = s.backoffs ?? 0;
      relayHtml = `<b>Signalling relays</b> (${s.published} sent, ${s.received} received, ${s.rejected} refused)<br>`
        + sig.sockets.map((ws) => { const x = st(ws); return `<span class="${x === 'open' ? 'ok' : x === 'connecting' ? 'dim' : 'bad'}">●</span> <span class="mono">${esc(ws.relayUrl)}</span> ${esc(x)}`; }).join('<br>');
      const piped = room.transport?.piped;
      if (piped) relayHtml += `<br><span class="dim">signalling route: ${piped.sent} batches sent, ${piped.received} datagrams received, ${piped.dropped} dropped</span>`;
    } else relayHtml = '<b>Signalling</b>: local (BroadcastChannel, this browser only)';
    $('relays').innerHTML = relayHtml;

    const list = [...peers.values()].filter((p) => !p.leftAt || t - p.leftAt < 60000).sort((a, b) => (b.joinedAt ? 1 : 0) - (a.joinedAt ? 1 : 0));
    $('peercount').textContent = `${linked.length} linked${heardOnly ? `, ${heardOnly} connecting` : ''}`;
    const cards = [];
    for (const p of list) {
      const isLinked = linked.includes(p.id);
      if (isLinked || room.transport?.has?.(p.id)) p.ice = await iceOf(p.id);
      const st = pingStats(p);
      const pres = p.presence ?? {};
      const state = p.leftAt ? `<span class="dim">left (${esc(p.leaveReason)})</span>`
        : isLinked ? `<span class="${room.route(p.id)?.kind === 'direct' ? 'ok' : 'warn'}">${esc(routeLabel(room.route(p.id)))}</span>`
          : `<span class="warn">connecting… heard ${secs(t - p.heardAt)} ago${p.rtcFailedAt ? ', WebRTC failed' : ''}</span>`;
      const hint = pairHint(p.presence);
      cards.push(`<div class="peer">
        <div class="top"><span class="name">${esc(nameOf(p.id))}</span><span class="mono dim">${short(p.id)}</span>${state}
          <span class="big ${pingCls(st.avg)}" style="margin-left:auto">${ms(st.last)}</span></div>
        ${isLinked ? spark(p.samples) : ''}
        <dl>
          <dt>Ping</dt><dd>avg <b class="${pingCls(st.avg)}">${ms(st.avg)}</b> · min ${ms(st.min)} · max ${ms(st.max)} · jitter ${ms(st.jitter)} · loss ${st.loss.toFixed(0)}% <span class="dim">(${p.recv}/${p.sent})</span></dd>
          <dt>Core RTT</dt><dd>${ms(isLinked ? room.rtt(p.id) : null)}</dd>
          <dt>WebRTC</dt><dd>${p.ice ? iceText(p.ice) : p.rtcFailedAt ? '<span class="bad">failed</span>' : '–'}${p.ice?.pair?.iceRttMs != null ? ` · ICE RTT ${p.ice.pair.iceRttMs} ms` : ''}</dd>
          ${p.sig.length ? `<dt>Signalling</dt><dd>${sigText(p)}</dd>` : ''}
          ${p.pairs.size ? `<dt>ICE checks</dt><dd>${checksText(p)}</dd>` : ''}
          <dt>Timing</dt><dd>heard→WebRTC ${secs(p.heardAt && p.rtcOpenAt ? p.rtcOpenAt - p.heardAt : null)} · heard→joined ${secs(p.heardAt && p.joinedAt ? p.joinedAt - p.heardAt : null)}</dd>
          <dt>Their side</dt><dd>${esc(clip(pres.dev, 40) || '–')} · NAT <b>${esc(clip(pres.nat, 20) || '?')}</b> · IPv6 ${pres.v6 ? 'yes' : 'no'}${pres.conn && pres.conn !== 'unknown' ? ` · ${esc(clip(pres.conn, 40))}` : ''}</dd>
        </dl>
        ${hint ? `<div class="note dim">${esc(hint)}</div>` : ''}
        ${p.pairs.size ? `<details><summary>candidate pairs</summary><div class="mono">${[...p.pairs.values()].map((q) => `${esc(q.pair)} ${esc(q.state)}: sent ${q.requestsSent}, answered ${q.responsesReceived}, received ${q.requestsReceived}`).join('<br>')}</div></details>` : ''}
        ${p.routeLog.length > 1 ? `<details><summary>route history</summary><div class="mono">${p.routeLog.map((r) => `${secs(r.at)}: ${esc(r.route)}`).join('<br>')}</div></details>` : ''}
      </div>`);
    }
    $('peers').innerHTML = cards.join('') || `<p class="dim">No peers yet. ${roomKind === 'global' ? 'Anyone who opens Net Lab and joins the global room shows up here.' : 'Share the link (button above) with someone.'}</p>`;
  } finally {
    rendering = false;
  }
}

// ---------- chat ----------

function addChat(from, text, via) {
  chat.push({ at: Date.now(), from, text, via });
  if (chat.length > 200) chat.shift();
  const div = document.createElement('div');
  div.innerHTML = `<span class="dim">${new Date().toLocaleTimeString()}</span> <b>${esc(from)}</b>: ${esc(text)}${via ? ` <span class="dim">(${esc(via)})</span>` : ''}`;
  $('chatlog').append(div);
  $('chatlog').scrollTop = 1e6;
}

$('chatform').addEventListener('submit', (e) => {
  e.preventDefault();
  const text = $('chatin').value.trim();
  if (!text || !room) return;
  $('chatin').value = '';
  const n = room.send({ t: 'chat', text: clip(text, 500) });
  addChat(`${myName()} (me)`, text, n ? `to ${n} peer${n === 1 ? '' : 's'}` : 'nobody linked yet');
});

// ---------- the report ----------

function report() {
  const t = now();
  return {
    tool: 'peerwire-netlab', at: new Date().toISOString(), page: location.origin + location.pathname, uptimeS: Math.round((Date.now() - t0) / 1000),
    me: { id: room?.self ?? null, name: myName(), device: device(), connection: connInfo(), userAgent: navigator.userAgent, network: myNet },
    room: room && {
      kind: roomKind, inRoomS: Math.round((t - joinedAt) / 1000),
      signaling: room.signaling.sockets
        ? { relays: room.signaling.sockets.map((ws) => ({ url: ws.relayUrl, state: ['connecting', 'open', 'closing', 'closed'][ws.readyState] })), stats: room.signaling.stats }
        : 'local',
      piped: room.transport?.piped ?? null, core: room.stats(),
    },
    peers: [...peers.values()].map((p) => ({
      id: p.id, name: nameOf(p.id), presence: p.presence, linked: !!room?.peers.includes(p.id), route: p.route, routeLog: p.routeLog,
      ping: pingStats(p), samples: p.samples.map(Math.round), pingsSent: p.sent, pongs: p.recv, lost: p.lost,
      coreRttMs: room?.peers.includes(p.id) ? room.rtt(p.id) : null, webrtc: p.ice,
      heardToRtcMs: p.heardAt && p.rtcOpenAt ? Math.round(p.rtcOpenAt - p.heardAt) : null,
      heardToJoinMs: p.heardAt && p.joinedAt ? Math.round(p.joinedAt - p.heardAt) : null,
      signaling: p.sig, iceChecks: [...p.pairs.values()],
      rtcFailed: !!p.rtcFailedAt, left: p.leftAt ? p.leaveReason : null,
    })),
    log: events.map((e) => ({ t: new Date(e.at).toISOString(), level: e.level, msg: e.msg })),
  };
}

async function copyReport() {
  const text = JSON.stringify(report(), null, 2);
  try {
    await navigator.clipboard.writeText(text);
    log('info', `report copied (${text.length} characters)`);
  } catch {
    downloadReport();
  }
}
function downloadReport() {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([JSON.stringify(report(), null, 2)], { type: 'application/json' }));
  a.download = `netlab-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  log('info', 'report downloaded');
}

// ---------- wiring ----------

$('name').value = params.get('name') ?? localStorage.getItem('netlab-name') ?? '';
$('global').onclick = () => enter('global');
$('create').onclick = () => enter('private', randomRoomId());
$('joinkey').onclick = () => {
  const key = parseKey($('key').value);
  if (!key) return ($('joinmsg').textContent = 'paste a room key or the full link');
  enter('private', key);
};
$('leave').onclick = async () => {
  await room?.leave();
  log('info', 'left the room');
  location.hash = '';
  location.reload();
};
$('share').onclick = async () => {
  const link = `${location.origin}${location.pathname}#key=${roomKey}`;
  try {
    if (navigator.share) await navigator.share({ title: 'Peerwire Net Lab room', url: link });
    else { await navigator.clipboard.writeText(link); log('info', 'room link copied'); }
  } catch {
    prompt('Room link', link);
  }
};
$('reprobe').onclick = reprobe;
$('copy').onclick = copyReport;
$('download').onclick = downloadReport;
addEventListener('pagehide', () => room?.leave());

// For tests and the console.
window.netlab = {
  get self() { return room?.self ?? null; },
  get key() { return roomKey; },
  peers: () => room?.peers.map((id) => ({ id, name: nameOf(id), route: room.route(id)?.kind ?? null, ping: pingStats(peerState(id)) })) ?? [],
  say: (text) => { $('chatin').value = text; $('chatform').requestSubmit(); },
  chat: () => chat.map((c) => `${c.from}: ${c.text}`),
  report,
  joinGlobal: () => enter('global'),
  joinPrivate: (key) => enter('private', key ?? randomRoomId()),
};

log('info', `Net Lab on ${device()}, connection ${connInfo()}`);
setInterval(tickPings, 250);
setInterval(render, 1000);
const hashKey = parseKey(location.hash);
reprobe();
if (hashKey) enter('private', hashKey);
else if (params.get('auto') === 'global') enter('global');

