// Net Lab: a connection diagnostics page on Peerwire rooms, not a game. Join a room
// by any name you choose ("office test", shared as a #room= link), the global room
// (everyone using Net Lab meets there) or a private room (a random key, shared as a
// #key= link), and every member links to every other one. For each
// peer it shows the route (direct WebRTC, relayed by a member, or over the
// signalling relays), how the WebRTC link was made (ICE candidate pair, the address
// it goes to, IPv4 or IPv6, UDP or TCP), the time it took, a birthday-punch test,
// and a ping measured by the page itself on whatever route is in use (min/avg/max,
// jitter, loss). It also probes this
// device's network (public addresses, IPv6, a NAT mapping estimate), can ask for the
// microphone so the browser shares this device's local addresses with named and
// private rooms (never the global one), shows the
// signalling relays' state, has a chat, logs every event, warning and error, and
// copies all of it as one JSON report.
//
//   ?signal=local      BroadcastChannel signalling (tabs of one browser; tests)
//   ?rtc=off           fail every WebRTC link of this tab, as behind a symmetric NAT
//   ?gw=a,b            hotspot gateways tried for a phone sharing mobile data
//                      (default 192.168.43.1 and 172.20.10.1, plus the address typed
//                      under "phone hotspot"); ?synth=off turns that off
//   ?relays=wss://a,…  other Nostr relays
//   ?name=…            the name; ?auto=global joins the global room
//   #room=…            joins the room of that name (a hash, so it never reaches the server)
//   #key=…             joins that private room

import { joinRoom, randomRoomId, DEFAULT_RELAYS } from '../src/index.js';
import { openFleet, openSpray, randomCredentials, sprayPorts, isPublicV4, FLEET_SIZE, SPRAY_PORTS } from '../src/birthday.js';

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
const RECENT = 'netlab-rooms', MAX_RECENT = 6;

const params = new URLSearchParams(location.search);
// Hotspot gateways that don't change: older Android (192.168.43.1) and iPhone
// (172.20.10.1), or ?gw=a,b; plus one typed under "phone hotspot", for the newer
// Androids that pick a random one. Tried for a peer whose host address starts with
// HOTSPOT_FROM (the CLAT range; ?gwfrom= in tests), or that has our public IPv4.
const isV4 = (s) => /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.test(s) && s.split('.').every((n) => +n <= 255);
const ipList = (s) => String(s ?? '').split(/[\s,]+/).filter(isV4);
const GW_DEFAULT = params.has('gw') ? ipList(params.get('gw')) : ['192.168.43.1', '172.20.10.1'];
const hotspotGateways = () => [...new Set([...ipList(localStorage.getItem('netlab-gw')), ...GW_DEFAULT])].slice(0, 4);
const HOTSPOT_FROM = params.get('gwfrom') ?? '192.0.0.';
const $ = (id) => document.getElementById(id);
const now = () => performance.now();
const short = (id) => String(id).slice(0, 6);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const ms = (v) => (v == null || !Number.isFinite(v) ? '–' : `${Math.round(v)} ms`);
const secs = (v) => (v == null ? '–' : `${(v / 1000).toFixed(1)} s`);
const clip = (s, n) => String(s ?? '').slice(0, n);
const t0 = Date.now();

let room = null;
let roomKind = null; // 'global' | 'named' | 'private'
let roomKey = null; // a private room's key
let roomName = null; // a named room's name, as its id (office-test)
let joinedAt = 0;
let myNet = null; // the result of probeNetwork()
const peers = new Map(); // peer id -> P (see peerState)
const chat = [];
const events = [];
let warnings = 0;
const relaySeen = new Map(); // relay url -> last state logged
let lastRejected = 0;
let lastBackoffs = 0;
let lastReconnects = 0;

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
  $('h-warn').title = 'see the event log';
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
        entry.base = `${c.relatedAddress ?? ''}:${c.relatedPort ?? ''}`; // the local socket (0.0.0.0:0 while hidden)
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
  // Per local socket and public IPv4 address: how many public ports the socket got from
  // the two servers. With the local addresses shown (mic permission) each interface has
  // its own socket, and its own public port, which is no sign of a symmetric NAT.
  const byIp = new Map();
  for (const c of res.srflx.filter((c) => !v6(c.address))) {
    const k = `${c.base} ${c.address}`;
    byIp.set(k, new Set([...(byIp.get(k) ?? []), c.port]));
  }
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
  return { name: myName(), dev: device(), conn: connInfo(), nat: myNet?.nat ?? 'testing', v6: !!myNet?.ipv6, mic: micState === 'granted' };
}

// ---------- microphone permission (shows local addresses) ----------
//
// Chrome hides this device's LAN addresses behind mDNS names (and Android Chrome
// offers only its mobile-data addresses, not its hotspot one) unless the page has mic
// or camera permission: then a new connection offers every interface's real address,
// so a laptop on a phone's hotspot may link to the phone directly. A connection made
// before the grant stays hidden, so a grant re-runs the network test and rejoins the
// room, which makes every link a new connection. Only the permission is needed: the
// microphone is closed at once. The grant is the site's, for later visits too, and anyone
// can open the global room: while the microphone is allowed, the global room is joined
// with privacy 'public-only', so its members never get the local addresses (MIC-1).
let micState = typeof navigator.mediaDevices?.getUserMedia === 'function' ? 'prompt' : 'unsupported';
let micChecked = Promise.resolve(); // watchMic()'s first query
let askingMic = false; // the button's own request, which rejoins by itself
let roomPrivacy = null; // the privacy the room was joined with
async function watchMic() {
  if (micState !== 'unsupported') {
    try {
      const st = await navigator.permissions.query({ name: 'microphone' });
      micState = st.state;
      st.onchange = () => {
        micState = st.state;
        renderMic();
        guardGlobal();
      };
    } catch { /* no 'microphone' query (Firefox): 'prompt' until the button is used */ }
  }
  renderMic();
}
function renderMic() {
  const shares = "this site's pages give the members of named and private rooms this device's local addresses (Wi-Fi, hotspot, VPN), on later visits too";
  const [cls, text] = {
    granted: ['ok', `allowed: ${shares}; remove the microphone in the site settings to stop`],
    denied: ['warn', 'not allowed: local addresses stay hidden (allow the microphone in the site settings)'],
    unsupported: ['faint', 'not available here (needs HTTPS)'],
  }[micState] ?? ['faint', `lets ${shares}, until you remove the microphone in the site settings; the microphone closes at once`];
  $('mic').disabled = micState === 'unsupported';
  $('micmsg').className = cls;
  $('micmsg').textContent = `${text}${roomKind === 'global' && micState !== 'unsupported' ? ' · local addresses are kept out of the global room' : ''}`;
}
// The global room never gets the local addresses: one joined without that guard while
// the microphone is now allowed (in another tab of this site, in the site settings, or
// as it joined) is joined again with it.
function guardGlobal() {
  if (askingMic || !room || roomKind !== 'global' || roomPrivacy === 'public-only' || micState !== 'granted') return;
  log('info', 'the microphone is allowed on this site: rejoining the global room without local addresses');
  rejoin();
}
async function useMic() {
  $('mic').disabled = true;
  askingMic = true;
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    for (const t of stream.getTracks()) t.stop();
    micState = 'granted';
  } catch (e) {
    askingMic = false;
    if (e?.name === 'NotAllowedError' || e?.name === 'SecurityError') micState = 'denied';
    log('warn', `microphone ${micState === 'denied' ? 'not allowed' : 'unavailable'}: ${e?.name ?? 'error'}${e?.message ? ` (${clip(e.message, 120)})` : ''}; local addresses stay hidden`);
    return renderMic();
  }
  renderMic();
  $('mic').disabled = true; // until the room is joined again
  log('info', `microphone allowed (closed again at once): new links in named and private rooms offer this device's local addresses (never the global room's); re-testing the network${room ? ' and rejoining the room' : ''}`);
  try {
    await reprobe();
    await rejoin();
  } finally {
    askingMic = false;
    renderMic();
  }
}

// Leaves the room and joins it again as a new member, so every link is a new connection.
async function rejoin() {
  if (!room) return;
  const old = room, kind = roomKind, key = kind === 'private' ? roomKey : roomName;
  for (const peer of peers.keys()) closeBday(peer);
  room = null;
  await old.leave();
  peers.clear();
  await enter(kind, key);
  if (!room) {
    // Could not join again: back to the join card, to try by hand.
    $('join').classList.remove('hidden');
    $('roombar').classList.add('hidden');
  }
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

// A room name as typed ("Office test!") as the room id it opens (office-test): lower
// case, accents dropped, anything else between letters and digits a dash.
function roomIdOf(s) {
  return String(s ?? '').normalize('NFKD').replace(/[\u0300-\u036f]/g, '').toLowerCase()
    .replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40).replace(/-+$/, '');
}
function parseRoom(s) {
  const m = String(s ?? '').match(/[#&?]room=([^&#]+)/);
  return m ? roomIdOf(decodeURIComponent(m[1])) || null : null;
}
function randomSuffix(n = 5) {
  const a = new Uint8Array(n);
  crypto.getRandomValues(a);
  return [...a].map((b) => 'abcdefghjkmnpqrstuvwxyz23456789'[b % 31]).join('');
}
function roomHint() {
  const id = roomIdOf($('roomname').value);
  if (!id) return void ($('roomhint').innerHTML = 'Any name works: everyone who types the same name lands in the same room.');
  const guessable = id.length < 12 && !/\d.*[a-z]|[a-z].*\d/.test(id.split('-').at(-1));
  $('roomhint').innerHTML = `Room <b>${esc(id)}</b>${guessable ? ' · a short name anyone could guess: <i>Make private</i> adds a random part' : ' · share this name or the room link'}`;
}
function recentRooms() {
  try { return JSON.parse(localStorage.getItem(RECENT) ?? '[]').filter((r) => typeof r === 'string').slice(0, MAX_RECENT); } catch { return []; }
}
function remember(id) {
  try { localStorage.setItem(RECENT, JSON.stringify([id, ...recentRooms().filter((r) => r !== id)].slice(0, MAX_RECENT))); } catch { /* private mode */ }
}
function showRecent() {
  const list = recentRooms();
  $('recentwrap').classList.toggle('hidden', !list.length);
  $('recent').innerHTML = list.map((r) => `<button type="button" class="chip" data-room="${esc(r)}">${esc(r)}</button>`).join('');
}
// The link that opens this room: a named room's name or a private room's key go in
// the hash, which browsers never send to the server.
function roomLink() {
  // Keeps the signalling choice (local tabs, or other relays), so the link lands in the same place.
  const q = new URLSearchParams([...params].filter(([k]) => k === 'signal' || k === 'relays'));
  if (roomKind === 'global') q.set('auto', 'global');
  const base = `${location.origin}${location.pathname}${q.size ? `?${q}` : ''}`;
  if (roomKind === 'named') return `${base}#room=${encodeURIComponent(roomName)}`;
  if (roomKind === 'private') return `${base}#key=${roomKey}`;
  return base;
}
const roomLabel = () => (roomKind === 'global' ? 'global' : roomKind === 'named' ? roomName : `private ${roomKey.slice(0, 4)}…`);

function parseKey(s) {
  s = String(s ?? '').trim();
  const m = s.match(/[#&?]key=([A-Za-z0-9_-]+)/);
  return m ? m[1] : /^[A-Za-z0-9_-]{8,64}$/.test(s) ? s : null;
}

// kind 'global'; 'named' with the room's id; 'private' with its key.
async function enter(kind, key) {
  if (room) return;
  const JOIN = ['global', 'create', 'joinkey', 'joinname'];
  for (const b of JOIN) $(b).disabled = true;
  try { localStorage.setItem('netlab-name', $('name').value.trim()); } catch { /* private mode */ }
  saveGw();
  roomKind = kind;
  roomKey = kind === 'private' ? key : null;
  roomName = kind === 'named' ? key : null;
  $('joinmsg').textContent = 'connecting to the signalling relays…';
  const relays = params.get('relays')?.split(',').filter(Boolean);
  const signaling = params.get('signal') === 'local' ? 'local' : 'nostr';
  await micChecked; // a stored grant must be known before choosing the global room's privacy
  const privacy = kind === 'global' && micState === 'granted' ? 'public-only' : 'direct';
  log('info', `joining the ${kind} room over ${signaling === 'local' ? 'local BroadcastChannel' : `Nostr (${(relays ?? DEFAULT_RELAYS).join(', ')})`}${params.get('rtc') === 'off' ? ', WebRTC disabled (?rtc=off)' : ''}${privacy === 'public-only' ? ', without local addresses (the microphone is allowed)' : ''}`);
  const started = now();
  try {
    room = await joinRoom({
      appId: APP_ID,
      // Named rooms get a prefix, so no name opens the global room or a private one.
      roomId: kind === 'global' ? GLOBAL_ROOM : kind === 'named' ? `named/${key}` : key,
      signaling,
      privacy,
      presence: { ...presence(), name: clip($('name').value.trim(), 20) || 'guest' },
      ...(relays?.length && { relays }),
      ...(params.get('rtc') === 'off' && { RTCPeerConnection: NoIcePc }),
    });
  } catch (e) {
    log('error', `could not join: ${e.message}`);
    $('joinmsg').textContent = `could not join: ${e.message}`;
    for (const b of JOIN) $(b).disabled = false;
    return;
  }
  joinedAt = now();
  roomPrivacy = privacy;
  if (!$('name').value.trim()) room.setPresence(presence());
  log('info', `joined in ${ms(joinedAt - started)} as ${short(room.self)}; waiting for peers`);
  hook();
  renderMic();
  guardGlobal(); // the microphone was allowed while this joined
  if (kind === 'private') history.replaceState(null, '', `${location.pathname}${location.search}#key=${key}`);
  if (kind === 'named') {
    history.replaceState(null, '', `${location.pathname}${location.search}#room=${encodeURIComponent(key)}`);
    remember(key);
  }
  $('join').classList.add('hidden');
  $('roomcard').classList.remove('hidden');
  $('sigcard').classList.remove('hidden');
  $('roombar').classList.remove('hidden');
  $('h-room').textContent = roomLabel();
  $('chatsend').disabled = false;
  $('joinmsg').textContent = '';
  render();
}

function peerState(peer) {
  let p = peers.get(peer);
  if (!p) {
    p = { id: peer, heardAt: null, rtcOpenAt: null, rtcFailedAt: null, joinedAt: null, leftAt: null, leaveReason: null, presence: null,
      route: null, routeLog: [], ice: null, sig: [], pairs: new Map(), pings: new Map(), seq: 0, lastPing: 0, samples: [], sent: 0, recv: 0, lost: 0, warned: new Set(), bday: null, pathLogged: null };
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
  // After a rejoin, the old room's late events don't touch the new room's peers.
  const live = room;
  const mine = () => room === live;
  const sig = room.signaling;
  const hello = sig.onHello;
  sig.onHello = (peer, meta) => {
    if (!mine()) return hello(peer, meta);
    const p = peerState(peer);
    if (!p.heardAt || p.leftAt) {
      p.heardAt = now();
      p.leftAt = null;
      log('info', `heard ${short(peer)} through signalling`);
    }
    hello(peer, meta);
  };
  const t = room.transport;
  if (t && 'trace' in t) t.trace = (...a) => { if (mine()) onTrace(...a); };
  if (t?.onPeer) {
    const onPeer = t.onPeer;
    t.onPeer = (peer, state, ...rest) => {
      if (!mine()) return onPeer(peer, state, ...rest);
      const p = peerState(peer);
      if (state === 'open') {
        p.rtcOpenAt = now();
        log('info', `WebRTC link to ${nameOf(peer)} open${p.heardAt ? ` ${secs(p.rtcOpenAt - p.heardAt)} after hearing it` : ''}`);
      } else if (state === 'failed') {
        p.rtcFailedAt = now();
        log('warn', `WebRTC to ${nameOf(peer)} failed (ICE found no working pair): trying a member relay, then the signalling relays`);
      } else if (state === 'closed') log('info', `WebRTC link to ${nameOf(peer)} closed`);
      onPeer(peer, state, ...rest);
    };
  }
  room.on('peer-join', (peer, pres) => {
    if (!mine()) return;
    const p = peerState(peer);
    p.presence = pres;
    p.joinedAt = now();
    p.leftAt = null;
    log('info', `${nameOf(peer)} joined (${clip(pres?.dev, 40)}, NAT ${clip(pres?.nat, 20)}, IPv6 ${pres?.v6 ? 'yes' : 'no'}${micText(pres)})${p.heardAt ? `, ${secs(p.joinedAt - p.heardAt)} after hearing it` : ''}`);
  });
  room.on('presence', (peer, pres) => { if (mine()) peerState(peer).presence = pres; });
  room.on('peer-leave', (peer, reason) => {
    if (!mine()) return;
    const p = peerState(peer);
    p.leftAt = now();
    p.leaveReason = reason;
    p.pings.clear();
    log(reason === 'left' || reason === 'closed' ? 'info' : 'warn', `${nameOf(peer)} left: ${reason}`);
  });
  room.on('message', (...a) => { if (mine()) onMessage(...a); });
}
// A peer's mic permission from its presence (an older Net Lab doesn't say).
const micText = (pres) => (typeof pres?.mic === 'boolean' ? `, mic ${pres.mic ? 'allowed' : 'not allowed'}` : '');

// The addresses in an offer, answer or late batch, as "1 IPv6 public, 2 hidden (mDNS)".
function addrText(d) {
  const parts = [[d.host6, 'IPv6 host'], [d.host4, 'IPv4 host'], [d.srflx6, 'IPv6 public'], [d.srflx4, 'IPv4 public'], [d.mdns, 'hidden (mDNS)'], [d.relay, 'TURN']]
    .filter(([n]) => n).map(([n, k]) => `${n} ${k}`);
  return parts.length ? parts.join(', ') : 'no addresses';
}

const SIG_TEXT = {
  'offer-sent': (d) => d.resend ? 'no answer yet: offer sent again' : `${d.restart ? 'ICE restart offer' : 'offer'} sent (${addrText(d)})`,
  'offer-received': (d) => `${d.restart ? 'ICE restart offer' : 'offer'} received (${addrText(d)})`,
  'answer-sent': (d) => d.resend ? 'their offer came again (our answer lost?): answer sent again' : `answer sent (${addrText(d)})`,
  'ice-restart': (d) => d.dialer ? `ICE lost (${d.reason}): restarting, try ${d.n}` : `ICE lost (${d.reason}): asked them to restart, try ${d.n}`,
  guesses: (d) => `their NAT is sequential symmetric: trying ${d.n} ports around ${d.ip}:${d.next} (step ${d.step})`,
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
  if (what === 'offer-received' || what === 'answer-received' || what === 'ice-batch-received') {
    tryHotspot(p, peer);
    setTimeout(() => { noteTheirHosts(p, peer); tryHotspot(p, peer); }, 500);
  }
  const bad = d.resend || what === 'ice-restart' || (what === 'ice-state' && (d.state === 'disconnected' || d.state === 'failed'));
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
  } else if (data.t === 'bday' && typeof data.step === 'string') {
    onBirthday(peer, data).catch((e) => bdayFail(peer, `error: ${e.message}`));
  }
}

// ---------- birthday punch (diagnostic) ----------
//
// Runs the browser birthday punch (src/birthday.js) between this tab and a peer, over
// the room's own signalling, and reports whether a direct WebRTC data channel forms
// across their real NATs. When it does and the room reaches that peer only by fallback
// (relayed by a member, or over signalling), both tabs hand the channel to the room
// (room.adoptChannel) and the room link goes direct over it; when the room link is
// direct already, the channel was a test and closes. The symmetric-NAT side opens a
// fleet of connections sharing one certificate and credentials; the cone side sprays.

const BDAY_TIMEOUT_MS = 70000;
const BDAY_COOLDOWN_MS = 300000; // one punch per peer per 5 min (NB48-1); reload to reset
let bdayBusy = null; // the peer whose punch holds this tab's one slot (NB48-1: one fleet at a time)
const rtcAvailable = () => params.get('rtc') !== 'off' && typeof RTCPeerConnection === 'function';
const bdayIce = () => (params.get('signal') === 'local' ? [] : PROBE_STUN);
// This device's own public IPv4 (from the network probe), for the hard side's answer.
const myProbedV4 = () => (myNet?.srflx ?? []).map((c) => c.address).find((a) => isPublicV4(a)) ?? null;
// The easy side's own public ip:port (its first server-reflexive candidate in `sdp`),
// which the hard fleet is told to target — instead of trusting the offer's candidate list.
function srflxAddr(sdp) {
  for (const m of (sdp ?? '').matchAll(/^a=candidate:\S+ \d+ udp \d+ (\d+\.\d+\.\d+\.\d+) (\d+) typ srflx/gim)) {
    if (isPublicV4(m[1])) return `${m[1]}:${m[2]}`;
  }
  return null;
}

// Which side each peer plays: the symmetric NAT is "hard" (opens the fleet), the
// cone is "easy" (sprays). The STUN "cone-like" reading is only about mapping, not
// filtering, and mobile CGNAT often reads cone-like but is really symmetric, so when
// neither is reported symmetric we fall back to hints: the cellular side is the likely
// symmetric one; else the larger id (so both sides agree). `want` ('hard'|'easy') from
// a retry overrides the guess, since the first roles may have been backwards.
function punchRoles(peer, want) {
  const sym = (n) => n === 'symmetric';
  const cell = (c) => /cellular/i.test(c ?? '');
  const them = peerState(peer).presence ?? {};
  const mine = myNet?.nat;
  if (sym(mine) && sym(them.nat)) return { bad: 'both sides are behind a symmetric NAT: a browser birthday punch needs one cone side' };
  let iAmHard, why;
  if (want) { iAmHard = want === 'hard'; why = 'you chose this role'; }
  else if (sym(mine) !== sym(them.nat)) { iAmHard = sym(mine); why = 'the symmetric side opens the fleet'; }
  else if (cell(connInfo()) !== cell(them.conn)) { iAmHard = cell(connInfo()); why = 'the cellular side is the likely symmetric one'; }
  else { iAmHard = room.self > peer; why = "neither NAT looks symmetric: a guess (press again to swap sides)"; }
  return { iAmHard, why, forced: !sym(mine) && !sym(them.nat) };
}

function bdayStatus(peer, status, extra = {}) {
  const p = peerState(peer);
  p.bday = { ...(p.bday ?? {}), status, at: now(), ...extra };
  render();
}
function bdayFail(peer, why) {
  closeBday(peer);
  bdayStatus(peer, 'failed', { why });
  log('warn', `birthday punch with ${nameOf(peer)}: ${why}`);
}
// Whether `b` is still `peer`'s punch in progress: not closed (a failure, the timeout,
// the peer's abort) nor left behind by a rejoin. Checked after every await of a punch's
// setup, so what it opened meanwhile is closed, not kept running (MIC-2).
const current = (peer, b) => peers.get(peer)?.bday === b && !!b.ctx;
function closeBday(peer) {
  if (bdayBusy === peer) bdayBusy = null; // NB48b-2: only the punch holding the slot frees it, not a refusal
  const b = peerState(peer).bday;
  if (!b?.ctx) return;
  try { b.ctx.easy?.pc.close(); } catch { /* already closed */ }
  for (const c of b.ctx.hard?.conns ?? []) try { c.pc.close(); } catch { /* already closed */ }
  clearTimeout(b.ctx.timer);
  b.ctx = null;
}

// Run a punch with `peer` — ONLY from a local click (the button), never from a signal.
// Accepts a peer's invite with the role it proposed, otherwise decides the role (and
// retries with it swapped after a failure, since mobile CGNAT reads cone-like but is
// often symmetric). One fleet at a time, one per peer per 5 min (NB48-1).
async function tryBirthday(peer) {
  if (!room || !rtcAvailable()) return;
  const prev = peerState(peer).bday;
  if (prev?.status === 'running') return;
  if (bdayBusy) return bdayFail(peer, 'another birthday punch is running — one at a time');
  if (prev?.lastRun && now() - prev.lastRun < BDAY_COOLDOWN_MS) {
    return bdayFail(peer, `one punch per peer per 5 min — wait ${Math.ceil((BDAY_COOLDOWN_MS - (now() - prev.lastRun)) / 1000)} s (or reload)`);
  }
  let iAmHard, why, initiated;
  if (prev?.status === 'invited' && prev.role) {
    iAmHard = prev.role === 'hard';
    why = "accepted the peer's request";
    initiated = false;
  } else {
    const want = prev?.status === 'failed' && prev.role ? (prev.role === 'hard' ? 'easy' : 'hard') : undefined;
    const roles = punchRoles(peer, want);
    if (roles.bad) return bdayFail(peer, roles.bad);
    iAmHard = roles.iAmHard;
    why = roles.why;
    initiated = true;
  }
  const role = iAmHard ? 'hard' : 'easy';
  const pending = prev?.pendingOffer; // a buffered offer from the easy initiator
  bdayBusy = peer;
  bdayStatus(peer, 'running', { role, startedAt: now(), lastRun: now(), ctx: { timer: setTimeout(() => bdayFail(peer, 'timed out (no direct channel formed)'), BDAY_TIMEOUT_MS) } });
  log('info', `birthday punch with ${nameOf(peer)}: starting as the ${role} side (${why})`);
  if (initiated) room.send({ t: 'bday', step: 'start', yourRole: iAmHard ? 'easy' : 'hard' }, { to: peer });
  if (!iAmHard) await beEasy(peer); // the hard side waits for the easy side's offer
  else if (pending) await onBirthday(peer, pending);
}
// A punch from a click or the console: one that throws (an accepted invite whose buffered
// offer openFleet refuses, say) fails at once and frees the slot, not after the 70 s timer.
const punch = (peer) => tryBirthday(peer).catch((e) => bdayFail(peer, `error: ${e.message}`));

async function onBirthday(peer, data) {
  const p = peerState(peer);
  if (!rtcAvailable()) return;
  if (data.step === 'start') {
    // NB48-1: a remote request never opens a fleet on its own — it only invites, and the
    // local user must press the button to accept. (If we already started, our role stands.)
    if (p.bday?.status === 'running') return;
    const role = data.yourRole === 'hard' ? 'hard' : 'easy';
    bdayStatus(peer, 'invited', { role });
    log('info', `${nameOf(peer)} wants a birthday punch (you would be the ${role} side) — press Birthday punch to accept`);
    return;
  }
  const b = p.bday;
  if (!b || b.status !== 'running' || !b.ctx) {
    // Before the local user accepts, keep the one offer so it isn't lost; ignore the rest.
    if (data.step === 'offer' && typeof data.sdp === 'string' && b?.status === 'invited') b.pendingOffer = data;
    return;
  }
  if (data.step === 'offer' && b.role === 'hard' && typeof data.sdp === 'string') {
    // The hard side: open the fleet answering the easy side's one offer, aimed only at the
    // one validated address it gave (openFleet strips the offer's candidates). Answer with our IP.
    const cert = await RTCPeerConnection.generateCertificate({ name: 'ECDSA', namedCurve: 'P-256' });
    if (!current(peer, b)) return;
    const creds = randomCredentials();
    const size = Number(params.get('fleet')) || FLEET_SIZE;
    const hard = await openFleet({ RTCPeerConnection, cert, creds, offerSdp: data.sdp, size, easyAddr: data.addr });
    if (!current(peer, b)) {
      for (const c of hard.conns) c.pc.close();
      return;
    }
    b.ctx.hard = hard;
    log('info', `birthday punch with ${nameOf(peer)}: opened ${hard.conns.length} sockets, answering`);
    room.send({ t: 'bday', step: 'answer', sdp: hard.answerSdp, ip: myProbedV4() }, { to: peer });
    hard.done.then((c) => bdayWon(peer, c)).catch(() => {});
  } else if (data.step === 'answer' && b.role === 'easy' && typeof data.sdp === 'string') {
    const ip = typeof data.ip === 'string' ? data.ip : null;
    if (!isPublicV4(ip)) return bdayFail(peer, 'the peer named no public IPv4 to spray'); // NB48-2
    const ports = sprayPorts(SPRAY_PORTS);
    log('info', `birthday punch with ${nameOf(peer)}: spraying ${ports.length} ports at ${ip}`);
    try {
      await b.ctx.easy.applyAnswer(data.sdp, ip, ports);
    } catch (e) {
      if (current(peer, b)) throw e; // else the punch ended meanwhile, and its end closed the connection
    }
  } else if (data.step === 'abort') {
    bdayFail(peer, `the peer stopped: ${clip(data.why, 80)}`);
  }
}

// The easy side: one connection, offer to the hard side with our own public address, spray on its answer.
async function beEasy(peer) {
  const b = peerState(peer).bday;
  const easy = await openSpray({ RTCPeerConnection, iceServers: bdayIce() });
  if (!current(peer, b)) return void easy.pc.close();
  b.ctx.easy = easy;
  room.send({ t: 'bday', step: 'offer', sdp: easy.offerSdp, addr: srflxAddr(easy.offerSdp) }, { to: peer });
  easy.done.then((c) => bdayWon(peer, c)).catch(() => {});
}

// The punch made a direct channel `{ pc, dc }`. If the room reaches the peer only by
// fallback, offer the channel to the room (the peer does the same on its side);
// otherwise, or if the peer doesn't take it, it was a test: close it after a moment.
async function bdayWon(peer, { pc, dc }) {
  const b = peerState(peer).bday;
  if (!b || b.status !== 'running') return;
  const ms = Math.round(now() - b.startedAt);
  const won = { ms, how: b.role === 'hard' ? "the peer's spray reached one of this side's fleet sockets" : "this side's spray reached one of the peer's fleet sockets" };
  // Read the pair that won (candidate types, the address it goes to) for the card and report.
  try {
    const sel = selectedPair(await pc.getStats());
    if (sel) Object.assign(won, { path: `${sel.local?.candidateType}↔${sel.remote?.candidateType}`, addr: addrOf(sel.remote) });
  } catch { /* closed under us */ }
  if (peerState(peer).bday !== b || b.status !== 'running') return; // timed out meanwhile
  // The winner leaves the punch's hands, so closeBday (which frees the slot and closes
  // the losers) leaves it open.
  if (b.ctx?.easy?.pc === pc) b.ctx.easy = null;
  if (b.ctx?.hard) b.ctx.hard.conns = b.ctx.hard.conns.filter((c) => c.pc !== pc);
  closeBday(peer);
  const before = room.route(peer);
  const what = `birthday punch with ${nameOf(peer)} WORKED: a direct channel${won.addr ? ` to ${won.addr}` : ''}${won.path ? ` (${won.path})` : ''} formed in ${secs(ms)}, as the ${b.role} side (${won.how})`;
  if (!before || before.kind === 'direct') {
    bdayStatus(peer, 'linked', { ...won, carried: false });
    log('info', `${what}. The room link is ${routeLabel(before)} already, so this was a test channel; closing it`);
    setTimeout(() => pc.close(), 1500);
    return;
  }
  bdayStatus(peer, 'linked', { ...won, carried: null });
  const mine = peerState(peer).bday; // bdayStatus makes a new object
  log('info', `${what}. Moving the room link onto it (it is ${routeLabel(before)} now)…`);
  const carried = await room.adoptChannel(peer, pc, dc);
  if (peerState(peer).bday === mine) bdayStatus(peer, 'linked', { carried });
  if (carried) log('info', `room link to ${nameOf(peer)} now goes over the birthday-punch channel (was ${routeLabel(before)})`);
  else {
    log('warn', `${nameOf(peer)} didn't take the birthday-punch channel (an older Net Lab, or its link to you is direct): it was a test channel; closing it. The room link stays ${routeLabel(room.route(peer))}`);
    pc.close();
  }
}

// The birthday-punch control + result in a peer card.
function bdayRow(p) {
  const b = p.bday;
  const roles = myNet ? punchRoles(p.id) : null;
  let label = 'Birthday punch', note = 'test a direct link across NATs', cls = 'faint';
  if (b?.status === 'running') { label = 'Punching…'; note = `running as the ${b.role} side…`; cls = 'warn'; }
  else if (b?.status === 'invited') { label = 'Accept punch'; note = `${nameOf(p.id)} asked — you'd be the ${b.role} side`; cls = 'warn'; }
  else if (b?.status === 'linked') {
    label = 'Punch again';
    const r = room.route(p.id);
    const use = r?.adopted ? 'the room link goes over it' : b.carried === null ? 'moving the room link onto it…' : b.carried ? `it closed: the room link is ${routeLabel(r)}` : `a test channel, the room link is ${routeLabel(r)}`;
    note = `worked: a direct channel${b.addr ? ` to ${b.addr}` : ''} in ${secs(b.ms)}${b.path ? ` · ${b.path}` : ''} · ${use}`;
    cls = 'ok';
  }
  else if (b?.status === 'failed') { label = 'Try again'; note = b.why; cls = 'bad'; }
  else if (roles?.bad) { note = roles.bad; }
  const disabled = b?.status === 'running' || (!b && roles?.bad);
  return `<div class="bdayrow"><button class="small" data-bday="${esc(p.id)}"${disabled ? ' disabled' : ''}>${label}</button><span class="${cls}">${esc(note)}</span></div>`;
}

// ---------- pinging ----------

function routeLabel(r) {
  if (!r) return 'not linked';
  if (r.kind === 'peer') return `relayed by ${nameOf(r.via)}`;
  if (r.kind === 'signaling') return 'over the signalling relays';
  return r.adopted ? 'direct (birthday-punch channel)' : 'direct';
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
  // A link a birthday punch's channel carries: that connection, not the failed one.
  const link = room.transport?.carrier?.(peer) ?? [...(room.transport?.links?.values() ?? [])].find((l) => l.peer === peer);
  if (!link?.pc) return null;
  const pc = link.pc;
  const out = { connection: pc.connectionState, ice: pc.iceConnectionState, gathering: pc.iceGatheringState, channel: link.dc?.readyState ?? null };
  try {
    const stats = await pc.getStats();
    let local = 0, remote = 0;
    const p = peerState(peer);
    stats.forEach((r) => {
      if (r.type === 'local-candidate') local++;
      if (r.type === 'remote-candidate') remote++;
      if (r.type === 'candidate-pair') notePair(p, r, stats);
    });
    out.candidates = { local, remote };
    const sel = selectedPair(stats);
    if (sel) {
      const { pair, local: l, remote: r } = sel;
      out.pair = {
        local: l?.candidateType, remote: r?.candidateType, protocol: l?.protocol,
        family: pairFamily(l, r) === 'v6' ? 'IPv6' : 'IPv4',
        remoteAddress: addrOf(r),
        iceRttMs: pair.currentRoundTripTime != null ? Math.round(pair.currentRoundTripTime * 1000) : null,
        bytesSent: pair.bytesSent, bytesReceived: pair.bytesReceived,
      };
    }
  } catch { /* the link closed under us */ }
  return out;
}

// The candidate pair a connection's stats say it uses (the transport's selected one,
// else a nominated pair that succeeded) with its two candidates, or null.
function selectedPair(stats) {
  let pair = null;
  stats.forEach((r) => { if (r.type === 'transport' && r.selectedCandidatePairId) pair = stats.get(r.selectedCandidatePairId); });
  if (!pair) stats.forEach((r) => { if (r.type === 'candidate-pair' && r.nominated && r.state === 'succeeded') pair = r; });
  return pair ? { pair, local: stats.get(pair.localCandidateId), remote: stats.get(pair.remoteCandidateId) } : null;
}

// A candidate's `ip:port` (`[v6]:port`), or null.
function addrOf(c) {
  const ip = c?.address ?? c?.ip;
  if (!ip) return null;
  return String(ip).includes(':') ? `[${ip}]:${c.port}` : `${ip}:${c.port}`;
}

// A candidate's IP family, 'v4' or 'v6'. Chrome leaves a hidden local address (mDNS)
// out of the stats, so a local candidate with none is on its pair's remote family.
const candAddr = (c) => String(c?.address ?? c?.ip ?? '');
const famOf = (c) => (candAddr(c).includes(':') ? 'v6' : 'v4');
const pairFamily = (l, rc) => famOf(candAddr(l) && !candAddr(l).endsWith('.local') ? l : rc);
// A candidate pair from the stats, as "host/v6 ↔ srflx/v6 2001:db8::1:5000".
const pairLabel = (l, rc) => `${l?.candidateType}/${pairFamily(l, rc)} ↔ ${rc?.candidateType}/${famOf(rc)} ${candAddr(rc)}:${rc?.port ?? ''}`;

// Chrome deletes candidate pairs that never got an answer (after ~15 s), so the
// counts are kept here: the most each pair ever reached.
const MAX_PAIRS = 24;
function notePair(p, r, stats) {
  const l = stats.get(r.localCandidateId), rc = stats.get(r.remoteCandidateId);
  const key = pairLabel(l, rc);
  let q = p.pairs.get(key);
  if (!q) {
    if (p.pairs.size >= MAX_PAIRS) return;
    q = { pair: key, protocol: l?.protocol, state: r.state, requestsSent: 0, responsesReceived: 0, requestsReceived: 0, responsesSent: 0 };
    p.pairs.set(key, q);
  }
  q.state = r.state;
  for (const k of ['requestsSent', 'responsesReceived', 'requestsReceived', 'responsesSent']) q[k] = Math.max(q[k], r[k] ?? 0);
}

// The other side's LAN addresses as its description gave them (counts alone can't
// tell a phone's hotspot address, 192.168.x.1, from its mobile-data one).
function noteTheirHosts(p, peer) {
  const t = room?.transport;
  const sdp = t?.links?.get(t.byPeer?.get(peer))?.pc?.remoteDescription?.sdp;
  if (typeof sdp !== 'string') return;
  const hosts = [...sdp.matchAll(/^a=candidate:\S+ \d+ \S+ \d+ (\S+) \d+ typ host/gm)].map((m) => m[1]).filter((h) => !h.endsWith('.local'));
  p.theirHosts = [...new Set(hosts)].slice(0, 8);
}

// A phone sharing its mobile data: Chrome on Android, without mic/camera
// permission, lists no interfaces and offers only its mobile-data address, which
// on IPv6-only carriers (Jio) is the CLAT one, 192.0.0.0/29. Its socket is bound to
// every address, so the same port answers on the hotspot side too (tested: direct,
// 6 ms): this adds a candidate at the hotspot gateway with that port. The phone
// learns this side's address from the first check that arrives (peer-reflexive),
// so the mDNS name it can't resolve doesn't matter. A peer with our public IPv4 is
// behind the same NAT, maybe the phone we tether to, even when it shows no CLAT
// host (an iPhone): its local port is the srflx rport, or else the public port.
function hotspotCandidates(sdp, gateways = hotspotGateways(), from = HOTSPOT_FROM, myPublic = []) {
  if (typeof sdp !== 'string') return [];
  const mid = /^a=mid:(\S+)/m.exec(sdp)?.[1] ?? '0';
  const ports = [];
  for (const m of sdp.matchAll(/^a=candidate:\S+ 1 udp \d+ (\S+) (\d+) typ host/gim)) if (m[1].startsWith(from)) ports.push(m[2]);
  for (const m of sdp.matchAll(/^a=candidate:\S+ 1 udp \d+ (\S+) (\d+) typ srflx(?: raddr \S+ rport (\d+))?/gim)) {
    if (myPublic.includes(m[1])) ports.push(m[3] && m[3] !== '0' ? m[3] : m[2]);
  }
  const out = [];
  const seen = new Set();
  for (const port of ports) {
    for (const gw of gateways) {
      const key = `${gw}:${port}`;
      if (seen.has(key) || out.length >= 6) continue;
      seen.add(key);
      out.push({ candidate: `candidate:pwgw${out.length} 1 udp 1677729535 ${gw} ${port} typ host generation 0`, sdpMid: mid, sdpMLineIndex: 0, gw: key });
    }
  }
  return out;
}

// This side's public IPv4 addresses, from the link's own description and the probe.
function myPublicV4(pc) {
  const sdp = pc?.localDescription?.sdp ?? '';
  const fromSdp = [...sdp.matchAll(/^a=candidate:\S+ \d+ udp \d+ (\S+) \d+ typ srflx/gim)].map((m) => m[1]);
  return [...new Set([...fromSdp, ...(myNet?.srflx ?? []).map((c) => c.address)])].filter(isV4);
}

const hotspotAdded = new WeakMap(); // pc -> the gateway:port candidates given to it
async function tryHotspot(p, peer) {
  if (params.get('synth') === 'off') return;
  const pc = room?.transport?.links?.get(room.transport.byPeer?.get(peer))?.pc;
  if (!pc) return;
  const added = hotspotAdded.get(pc) ?? new Set();
  hotspotAdded.set(pc, added);
  const fresh = [];
  for (const { gw, ...c } of hotspotCandidates(pc.remoteDescription?.sdp, hotspotGateways(), HOTSPOT_FROM, myPublicV4(pc))) {
    if (added.has(gw)) continue;
    added.add(gw);
    try {
      await pc.addIceCandidate(c);
      fresh.push(gw);
    } catch (e) {
      warnOnce(p, `gw ${gw}`, `could not add hotspot address ${gw}: ${e.message}`);
    }
  }
  if (!fresh.length) return;
  p.hotspot = [...new Set([...(p.hotspot ?? []), ...fresh])].slice(0, 8);
  log('info', `${nameOf(peer)} may be a phone sharing mobile data: also trying hotspot address(es) ${fresh.join(', ')}`);
}

// The offer/answer exchange, as "offer sent 0.2 s · answer received 1.9 s · 1 resend".
function sigText(p) {
  const first = (w) => p.sig.find((e) => e.what === w && !e.resend);
  const resends = p.sig.filter((e) => e.resend).length;
  const parts = [['offer-sent', 'offer sent'], ['offer-received', 'offer received'], ['answer-sent', 'answer sent'], ['answer-received', 'answer received'], ['ice-batch-received', 'late addresses received']]
    .map(([w, label]) => { const e = first(w); return e ? `${label}${e.at != null ? ` ${secs(e.at)}` : ''}` : null; }).filter(Boolean);
  const theirs = first('offer-received') ?? first('answer-received');
  // With trickle (local signalling) candidates come separately, not in the description.
  if (theirs && !room.transport?.trickle) parts.push(`their addresses: ${esc(addrText(theirs))}${p.theirHosts?.length ? ` (LAN ${esc(p.theirHosts.join(', '))})` : ''}`);
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

// "203.0.113.7:41000 (IPv4 UDP, your public (STUN) ↔ their public (peer-seen))", for the log.
function pathLine(p) {
  const kind = (k) => KIND[k] ?? k;
  return `${p.remoteAddress ?? '?'} (${p.family} ${String(p.protocol ?? '').toUpperCase()}, your ${kind(p.local)} ↔ their ${kind(p.remote)})`;
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
  const w = 300, h = 40, max = Math.max(...samples, 50);
  const pts = samples.map((v, i) => `${((i / (samples.length - 1)) * w).toFixed(1)},${(h - 2 - (v / max) * (h - 4)).toFixed(1)}`).join(' ');
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polyline fill="none" stroke="#6e8cff" stroke-width="1.5" vector-effect="non-scaling-stroke" points="${pts}"/></svg>`;
}
const pingCls = (v) => (v == null ? 'dim' : v < 150 ? 'ok' : v < SLOW_MS ? 'warn' : 'bad');

const bytes = (n) => (n == null ? '–' : n < 1024 ? `${n} B` : n < 1 << 20 ? `${(n / 1024).toFixed(1)} KB` : `${(n / (1 << 20)).toFixed(2)} MB`);
const hue = (id) => parseInt(String(id).slice(0, 4), 16) % 360 || 220;
const tile = (k, v, sub = '', cls = '') => `<div class="tile"><div class="k">${k}</div><div class="v ${cls}">${v}</div><div class="s">${sub || '&nbsp;'}</div></div>`;
const metric = (k, v, cls = '') => `<div class="metric"><span>${k}</span><b class="${cls}">${v}</b></div>`;
const routeTone = (r) => (!r ? 'warn' : r.kind === 'direct' ? 'ok' : r.kind === 'peer' ? 'warn' : 'bad');

let rendering = false;
async function render() {
  if (!room || rendering) return;
  rendering = true;
  try {
    const t = now();
    const linked = room.peers;
    const stats = linked.map((id) => [id, pingStats(peerState(id))]);
    const avgs = stats.map(([, s]) => s.avg).filter((v) => v != null);
    const roomAvg = avgs.length ? avgs.reduce((a, b) => a + b, 0) / avgs.length : null;
    const worst = stats.filter(([, s]) => s.avg != null).sort((a, b) => b[1].avg - a[1].avg)[0];
    const routes = { direct: 0, peer: 0, signaling: 0 };
    for (const id of linked) { const k = room.route(id)?.kind; if (k) routes[k]++; }
    $('h-room').textContent = roomLabel();
    $('h-peers').textContent = `${linked.length} linked`;
    $('h-peers').className = `pill ${linked.length ? 'ok' : ''}`;
    $('h-ping').textContent = `ping ${ms(roomAvg)}`;
    $('h-ping').className = `pill ${roomAvg == null ? '' : roomAvg < 150 ? 'ok' : roomAvg < SLOW_MS ? 'warn' : 'bad'}`;

    const heardOnly = [...peers.values()].filter((p) => p.heardAt && !p.joinedAt && !p.leftAt).length;
    const core = room.stats();
    let sent = 0, recv = 0, lost = 0, tx = 0, rx = 0;
    for (const id of linked) {
      const p = peerState(id);
      sent += p.recv + p.lost;
      lost += p.lost;
      tx += p.ice?.pair?.bytesSent ?? 0;
      rx += p.ice?.pair?.bytesReceived ?? 0;
    }
    recv = sent - lost;
    const loss = sent ? (100 * lost) / sent : null;
    $('roomstats').innerHTML = [
      tile('Peers linked', linked.length, heardOnly ? `${heardOnly} connecting` : roomKind === 'global' ? 'global room' : 'in this room', linked.length ? 'ok' : ''),
      tile('Average ping', ms(roomAvg), `over ${avgs.length} peer${avgs.length === 1 ? '' : 's'}`, pingCls(roomAvg)),
      tile('Worst ping', ms(worst?.[1].avg), worst ? esc(nameOf(worst[0])) : '', pingCls(worst?.[1].avg)),
      tile('Packet loss', loss == null ? '–' : `${loss.toFixed(1)}%`, `${recv} of ${sent} pings back`, loss == null ? '' : loss < 2 ? 'ok' : loss < 10 ? 'warn' : 'bad'),
      tile('Routes', `${routes.direct}<span class="dim" style="font-size:13px"> direct</span>`, `${routes.peer} relayed · ${routes.signaling} via signalling`, routes.peer + routes.signaling ? 'warn' : ''),
      tile('WebRTC traffic', bytes(tx + rx), `${bytes(tx)} out · ${bytes(rx)} in`),
      tile('Packets', core.txPackets + core.rxPackets, `${core.txPackets} out · ${core.rxPackets} in · ${core.retransmits} resent`),
      tile('In room', secs(t - joinedAt), `as ${esc(myName())}`),
    ].join('');

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
      if ((s.reconnects ?? 0) > lastReconnects) log('info', `a signalling relay reconnected (${s.reconnects} so far): announcing us again`);
      lastReconnects = s.reconnects ?? 0;
      relayHtml = sig.sockets.map((ws) => {
        const x = st(ws);
        return `<div class="relay"><span class="ldot ${x === 'open' ? 'ok' : x === 'connecting' ? '' : 'bad'}"></span><span class="mono">${esc(ws.relayUrl.replace(/^wss:\/\//, ''))}</span><span class="dim">${esc(x)}</span></div>`;
      }).join('');
      relayHtml += `<dl style="margin-top:8px"><dt>Events</dt><dd>${s.published} sent · ${s.received} received${s.rejected ? ` · <span class="warn">${s.rejected} refused</span>` : ''}</dd>
        <dt>Reconnects</dt><dd>${s.reconnects ?? 0}${s.backoffs ? ` · <span class="warn">${s.backoffs} rate-limit pauses</span>` : ''}</dd>`;
      const piped = room.transport?.piped;
      if (piped) relayHtml += `<dt>Fallback route</dt><dd>${piped.sent} batches sent · ${piped.received} datagrams received${piped.dropped ? ` · ${piped.dropped} dropped` : ''}</dd>`;
      const tr = room.transport;
      if (tr) relayHtml += `<dt>WebRTC signals</dt><dd>${tr.resent ?? 0} resent · ${tr.restarts ?? 0} ICE restarts${tr.badSignals ? ` · ${tr.badSignals} refused` : ''}</dd>`;
      relayHtml += '</dl>';
    } else relayHtml = '<p class="dim" style="margin:0">Local (BroadcastChannel): tabs of this browser only.</p>';
    $('relays').innerHTML = relayHtml;

    const list = [...peers.values()].filter((p) => !p.leftAt || t - p.leftAt < 60000).sort((a, b) => (b.joinedAt ? 1 : 0) - (a.joinedAt ? 1 : 0));
    $('peercount').textContent = `${linked.length} linked${heardOnly ? `, ${heardOnly} connecting` : ''}`;
    const cards = [];
    for (const p of list) {
      const isLinked = linked.includes(p.id);
      if (isLinked || room.transport?.has?.(p.id)) p.ice = await iceOf(p.id);
      const st = pingStats(p);
      const pres = p.presence ?? {};
      const r = room.route(p.id);
      const state = p.leftAt ? `<span class="pill">left: ${esc(p.leaveReason)}</span>`
        : isLinked ? `<span class="pill ${routeTone(r)}">${esc(routeLabel(r))}</span>`
          : `<span class="pill warn">connecting · heard ${secs(t - p.heardAt)} ago${p.rtcFailedAt ? ' · WebRTC failed' : ''}</span>`;
      const hint = pairHint(p.presence);
      const pair = p.ice?.pair;
      const restarts = p.sig.filter((e) => e.what === 'ice-restart').length;
      const guess = p.sig.find((e) => e.what === 'guesses');
      const name = nameOf(p.id);
      // Log where a direct room link goes, once per path (it can move on an ICE restart).
      const direct = isLinked && r?.kind === 'direct' && pair ? pathLine(pair) : null;
      if (direct && direct !== p.pathLogged) {
        p.pathLogged = direct;
        log('info', `route to ${name}: direct to ${direct}${r.adopted ? ', over the birthday-punch channel' : ''}`);
      }
      cards.push(`<div class="peer${p.leftAt ? ' gone' : ''}">
        <div class="phead">
          <span class="avatar" style="background:hsl(${hue(p.id)} 45% 38%)">${esc(name.slice(0, 1).toUpperCase())}</span>
          <div class="pname"><b>${esc(name)}</b><span class="mono">${short(p.id)} · ${esc(clip(pres.dev, 40) || 'device unknown')}</span></div>
          ${state}
          <div class="pbig"><span class="big ${pingCls(st.last)}">${ms(st.last)}</span><span class="dim">latest ping</span></div>
        </div>
        ${isLinked ? spark(p.samples) : ''}
        <div class="metrics">
          ${metric('Avg', ms(st.avg), pingCls(st.avg))}${metric('Min', ms(st.min))}${metric('Max', ms(st.max))}${metric('Jitter', ms(st.jitter))}
          ${metric('Loss', `${st.loss.toFixed(0)}%`, st.loss < 2 ? '' : st.loss < 10 ? 'warn' : 'bad')}${metric('Core RTT', ms(isLinked ? room.rtt(p.id) : null))}
          ${metric('ICE RTT', pair?.iceRttMs != null ? `${pair.iceRttMs} ms` : '–')}${metric('Pings', `${p.recv}/${p.sent}`)}
        </div>
        <dl>
          <dt>Path</dt><dd>${p.ice ? iceText(p.ice) : p.rtcFailedAt ? '<span class="bad">WebRTC failed</span>' : '–'}</dd>
          ${pair ? `<dt>Traffic</dt><dd>${bytes(pair.bytesSent)} out · ${bytes(pair.bytesReceived)} in</dd>` : ''}
          <dt>Timing</dt><dd>heard → WebRTC ${secs(p.heardAt && p.rtcOpenAt ? p.rtcOpenAt - p.heardAt : null)} · heard → joined ${secs(p.heardAt && p.joinedAt ? p.joinedAt - p.heardAt : null)}</dd>
          ${p.sig.length ? `<dt>Signalling</dt><dd>${sigText(p)}</dd>` : ''}
          ${p.pairs.size ? `<dt>ICE checks</dt><dd>${checksText(p)}</dd>` : ''}
          ${restarts ? `<dt>ICE restarts</dt><dd class="warn">${restarts}</dd>` : ''}
          ${guess ? `<dt>Port guesses</dt><dd>${guess.n} ports around <span class="mono">${esc(guess.ip)}:${guess.next}</span> (step ${guess.step})</dd>` : ''}
          <dt>Their network</dt><dd>NAT <b>${esc(clip(pres.nat, 20) || '?')}</b> · IPv6 ${pres.v6 ? 'yes' : 'no'}${typeof pres.mic === 'boolean' ? ` · mic ${pres.mic ? '<span class="ok">allowed</span>' : 'not allowed'}` : ''}${pres.conn && pres.conn !== 'unknown' ? ` · ${esc(clip(pres.conn, 40))}` : ''}</dd>
        </dl>
        ${hint ? `<div class="hintbox">${esc(hint)}</div>` : ''}
        ${isLinked && rtcAvailable() ? bdayRow(p) : ''}
        ${p.pairs.size ? `<details data-d="pairs-${esc(p.id)}"><summary>Candidate pairs (${p.pairs.size})</summary><div class="mono">${[...p.pairs.values()].map((q) => `${esc(q.pair)} ${esc(q.state)}: sent ${q.requestsSent}, answered ${q.responsesReceived}, received ${q.requestsReceived}`).join('<br>')}</div></details>` : ''}
        ${p.routeLog.length > 1 ? `<details data-d="routes-${esc(p.id)}"><summary>Route history (${p.routeLog.length})</summary><div class="mono">${p.routeLog.map((x) => `${secs(x.at)}: ${esc(x.route)}`).join('<br>')}</div></details>` : ''}
      </div>`);
    }
    // render() rebuilds this block every second, which would collapse a <details>
    // (candidate pairs, route history) the user opened: carry the open ones across.
    const open = new Set([...$('peers').querySelectorAll('details[open]')].map((d) => d.dataset.d));
    $('peers').innerHTML = cards.join('') || `<p class="dim">No peers yet. ${roomKind === 'global' ? 'Anyone who opens Net Lab and joins the global room shows up here.' : 'Send the room link (Copy link, at the top) to someone, or have them type the same room name.'}</p>`;
    for (const d of $('peers').querySelectorAll('details')) if (open.has(d.dataset.d)) d.open = true;
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
    me: { id: room?.self ?? null, name: myName(), device: device(), connection: connInfo(), userAgent: navigator.userAgent, mediaPermission: micState, network: myNet },
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
      signaling: p.sig, theirHosts: p.theirHosts ?? [], hotspotTried: p.hotspot ?? [], iceChecks: [...p.pairs.values()],
      rtcFailed: !!p.rtcFailedAt, left: p.leftAt ? p.leaveReason : null,
      birthday: p.bday && { status: p.bday.status, role: p.bday.role, ms: p.bday.ms, addr: p.bday.addr, path: p.bday.path, how: p.bday.how, why: p.bday.why, carried: p.bday.carried },
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

let savedName = '';
try { savedName = localStorage.getItem('netlab-name') ?? ''; } catch { /* private mode */ }
$('name').value = params.get('name') ?? savedName;
$('gw').value = localStorage.getItem('netlab-gw') ?? '';
function saveGw() {
  const raw = $('gw').value.trim();
  const ok = ipList(raw);
  if (raw && !ok.length) return ($('gwmsg').textContent = 'not an IPv4 address, e.g. 10.129.132.170');
  $('gwmsg').textContent = ok.length ? `also trying ${ok.join(', ')} for a phone sharing its mobile data` : '';
  if (ok.length) localStorage.setItem('netlab-gw', ok.join(','));
  else localStorage.removeItem('netlab-gw');
}
$('gw').onchange = saveGw;
$('nameform').onsubmit = (e) => {
  e.preventDefault();
  const id = roomIdOf($('roomname').value);
  if (!id) return void ($('joinmsg').textContent = 'Type a room name, or pick a recent one.');
  enter('named', id);
};
$('roomname').oninput = roomHint;
$('private').onclick = () => {
  const base = roomIdOf($('roomname').value) || 'room';
  $('roomname').value = `${base.replace(/-[a-z0-9]{5}$/, '')}-${randomSuffix()}`;
  roomHint();
};
$('recent').onclick = (e) => {
  const r = e.target.dataset?.room;
  if (r) enter('named', r);
};
$('onlywarn').onclick = () => {
  const on = $('onlywarn').getAttribute('aria-pressed') !== 'true';
  $('onlywarn').setAttribute('aria-pressed', String(on));
  $('log').classList.toggle('only-warn', on);
};
$('global').onclick = () => enter('global');
$('create').onclick = () => enter('private', randomRoomId());
$('joinkey').onclick = () => {
  const named = parseRoom($('key').value);
  if (named) return enter('named', named);
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
  const link = roomLink();
  try {
    // A phone shares through its own sheet (messages, chat apps); elsewhere, copy.
    if (navigator.share && matchMedia('(pointer: coarse)').matches) await navigator.share({ title: 'Net Lab room', url: link });
    else {
      await navigator.clipboard.writeText(link);
      log('info', 'room link copied');
      $('share').textContent = 'Copied';
      setTimeout(() => ($('share').textContent = 'Copy link'), 1500);
    }
  } catch (e) {
    if (e?.name !== 'AbortError') prompt('Room link', link);
  }
};
$('reprobe').onclick = reprobe;
$('mic').onclick = useMic;
$('copy').onclick = copyReport;
$('download').onclick = downloadReport;
// The peer cards are rebuilt each second; one delegated listener on the container survives.
$('peers').addEventListener('click', (e) => {
  const id = e.target.closest('[data-bday]')?.dataset.bday;
  if (id) punch(id);
});
addEventListener('pagehide', () => room?.leave());

// For tests and the console.
window.netlab = {
  get self() { return room?.self ?? null; },
  get key() { return roomKey; },
  peers: () => room?.peers.map((id) => ({ id, name: nameOf(id), route: room.route(id)?.kind ?? null, ping: pingStats(peerState(id)) })) ?? [],
  say: (text) => { $('chatin').value = text; $('chatform').requestSubmit(); },
  chat: () => chat.map((c) => `${c.from}: ${c.text}`),
  report,
  hotspotCandidates,
  pairLabel,
  joinGlobal: () => enter('global'),
  joinNamed: (name) => enter('named', roomIdOf(name)),
  punch: (i = 0) => punch(room.peers[i]),
  // Tests: as if a birthday punch with peer i had just made the channel `{ pc, dc }`
  // (loopback has one NAT, so a real punch can't meet there).
  punchWon: (i, chan) => {
    const peer = room.peers[i];
    bdayStatus(peer, 'running', { role: 'hard', startedAt: now(), ctx: {} });
    return bdayWon(peer, chan);
  },
  birthday: () => room?.peers.map((id) => ({ id, name: nameOf(id), ...(peerState(id).bday ?? { status: null }) })).map(({ ctx, ...b }) => b) ?? [],
  get room() { return roomKind && { kind: roomKind, label: roomLabel(), link: roomLink() }; },
  roomIdOf,
  joinPrivate: (key) => enter('private', key ?? randomRoomId()),
};

log('info', `Net Lab on ${device()}, connection ${connInfo()}`);
setInterval(tickPings, 250);
setInterval(render, 1000);
const hashKey = parseKey(location.hash);
const hashRoom = parseRoom(location.hash);
$('roomname').value = hashRoom ?? params.get('room') ?? '';
roomHint();
showRecent();
micChecked = watchMic();
reprobe();
if (hashKey) enter('private', hashKey);
else if (hashRoom) enter('named', hashRoom);
else if (params.get('auto') === 'global') enter('global');

