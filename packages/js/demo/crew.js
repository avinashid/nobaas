// Crew: a tiny Among Us over Peerwire rooms. No game server: peers meet through
// Nostr relays (or a BroadcastChannel with ?signal=local) and link by WebRTC.
//
// The host is authoritative and is also every player's relay: players send only
// to the host (positions unreliable, actions reliable) and the host sends every
// player the snapshot and the game state. So a player needs a working link to the
// host only, not to everyone; links between players in the mesh go unused.
//
//   player -> host   pos {x,y} (unreliable, 20 Hz), task/kill/report/emergency/vote/chat
//   host -> all      snap [[id,x,y]...] (unreliable, 15 Hz), state (reliable, on change), chat
//   host -> one      role {role, tasks, partners} at the start, cd {ms} to an impostor
//
// Every message from a peer is untrusted: the host checks phase, roles, distances
// and cooldowns before acting on one.
//
// Two peers WebRTC can't link (symmetric NAT on both sides) still play: joinRoom
// relays them through another player, else over the Nostr relays (slower), with
// no setup on either side. The network panel shows each peer's route.
// ?turn=turn:host:3478&tu=user&tp=pass adds a TURN server; ?rtc=off makes every
// WebRTC link of this tab fail, as between two symmetric NATs (tests).

import { joinRoom, DEFAULT_ICE_SERVERS } from '../src/index.js';

const APP_ID = 'peerwire-crew';
const W = 1600, H = 1000; // world
const R = 16; // player radius
const SPEED = 190; // px/s
const KILL_RANGE = 70, REPORT_RANGE = 90, USE_RANGE = 45, BUTTON_RANGE = 60;
const SLACK = 40; // the host's tolerance for position lag on kill/report
const KILL_CD = 20000, FIRST_CD = 12000, MEETING_MS = 45000, TASK_MS = 2000, TASKS_EACH = 4;
const VISION = { crew: 230, impostor: 320 };
const COLORS = ['#e5484d', '#3e63dd', '#46a758', '#f76b15', '#ffe629', '#8e4ec6', '#12a594', '#d6409f', '#f5f5f5', '#8d6e63'];
const ROOMS = [
  ['Cafeteria', 600, 60, 400, 260], ['Weapons', 1200, 80, 260, 200], ['Navigation', 1300, 420, 240, 200],
  ['Storage', 1100, 700, 300, 220], ['Electrical', 500, 700, 280, 220], ['Medbay', 160, 380, 260, 200],
  ['Reactor', 80, 60, 300, 220], ['Admin', 820, 430, 240, 170],
];
const HALLS = [
  [380, 150, 220, 70], [1000, 150, 200, 70], [1330, 280, 70, 140], [1330, 620, 70, 80], [780, 780, 320, 70],
  [290, 580, 70, 270], [290, 780, 210, 70], [230, 280, 70, 100], [870, 320, 70, 110],
];
const WALK = [...ROOMS.map((r) => r.slice(1)), ...HALLS];
const BUTTON = { x: 800, y: 190 };
const TASKS = [
  ['Fix wiring', 560, 760], ['Upload data', 1000, 470], ['Empty garbage', 650, 280], ['Calibrate', 1480, 520],
  ['Start reactor', 140, 120], ['Scan', 220, 440], ['Fuel engines', 1340, 860], ['Clear asteroids', 1400, 140],
  ['Divert power', 720, 880], ['Align engine', 320, 240], ['Swipe card', 860, 560], ['Clean vent', 380, 540],
];

const params = new URLSearchParams(location.search);
const $ = (id) => document.getElementById(id);
const canvas = $('view');
const ctx = canvas.getContext('2d');
const now = () => performance.now();
const dist = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
const walkable = (x, y) => [[-R, -R], [R, -R], [-R, R], [R, R]].every(([dx, dy]) =>
  WALK.some(([rx, ry, rw, rh]) => x + dx >= rx && x + dx <= rx + rw && y + dy >= ry && y + dy <= ry + rh));
const spawn = (i) => ({ x: BUTTON.x + Math.cos(i * 0.9) * 90, y: BUTTON.y + 20 + Math.sin(i * 0.9) * 45 });
const short = (id) => id.slice(0, 6);

let room = null;
let isHost = false;
let hostId = null;
let myName = '';
let started = 0;
const seen = new Map(); // peers heard through signalling -> first time
const pos = new Map(); // id -> {x, y}
let me = { x: BUTTON.x, y: BUTTON.y + 60 };
let S = { phase: 'none', round: 0, players: {}, bodies: [], taskTotal: 0, taskDone: 0, meeting: null, result: null, note: '' };
let role = null; // {role, tasks, partners}
const done = new Set(); // my finished task indexes
let killReady = 0; // local time the kill cooldown ends
let meetingEnds = 0;
let holding = false, holdFor = -1, holdMs = 0;
let myVote = null;
let seenRound = -1, seenPhase = 'none', seenNote = ''; // the last state applied, for transitions
const keys = new Set();
const chatLines = [];

// ---- host state (only on the host) ----
const G = { impostors: new Set(), tasks: new Map(), done: new Map(), cd: new Map(), votes: new Map(), used: new Set(), meetingEnds: 0 };

function log(line) {
  const li = document.createElement('li');
  li.textContent = line;
  $('log').prepend(li);
  while ($('log').children.length > 12) $('log').lastChild.remove();
}

// ---------- joining ----------

// An RTCPeerConnection that never learns or gives out a candidate, so ICE fails.
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

function iceServers() {
  const turn = params.get('turn');
  if (!turn) return undefined;
  return [...DEFAULT_ICE_SERVERS, { urls: turn, username: params.get('tu') ?? '', credential: params.get('tp') ?? '' }];
}

async function enter(asHost) {
  myName = $('name').value.trim().slice(0, 14) || `player-${Math.floor(Math.random() * 900 + 100)}`;
  const code = $('code').value.trim();
  if (!code) return ($('lobbymsg').textContent = 'type a room code, or click random');
  localStorage.setItem('crew-name', myName);
  $('host').disabled = $('join').disabled = true;
  $('lobbymsg').textContent = 'connecting to the signalling relays…';
  isHost = asHost;
  try {
    room = await joinRoom({
      appId: APP_ID, roomId: code,
      signaling: params.get('signal') === 'local' ? 'local' : 'nostr',
      presence: { name: myName, host: asHost },
      iceServers: iceServers(),
      ...(params.get('rtc') === 'off' && { RTCPeerConnection: NoIcePc }),
    });
  } catch (e) {
    $('host').disabled = $('join').disabled = false;
    return ($('lobbymsg').textContent = `could not join: ${e.message}`);
  }
  started = now();
  const hello = room.signaling.onHello;
  room.signaling.onHello = (peer, meta) => {
    if (!seen.has(peer)) seen.set(peer, now());
    hello(peer, meta);
  };
  room.on('peer-join', onPeerJoin);
  room.on('peer-leave', onPeerLeave);
  room.on('message', onMessage);
  history.replaceState(null, '', `?${new URLSearchParams({ ...Object.fromEntries(params), room: code })}`);
  $('lobby').classList.add('hidden');
  $('me').innerHTML = `<b>${esc(myName)}</b> <span class="dim">${short(room.self)}</span><br>room <code>${esc(code)}</code>`;
  if (isHost) {
    hostId = room.self;
    S = { ...S, phase: 'lobby', players: { [room.self]: { name: myName, color: COLORS[0], alive: true } } };
    pos.set(room.self, me);
    publish();
    log('hosting: share the room code');
  } else {
    S.phase = 'finding';
    log('looking for the host…');
  }
  render();
}

function onPeerJoin(peer, presence) {
  log(`linked to ${presence?.name ?? short(peer)}${presence?.host ? ' (host)' : ''}`);
  if (isHost) {
    const name = String(presence?.name ?? 'player').slice(0, 14);
    const used = new Set(Object.values(S.players).map((p) => p.color));
    const color = COLORS.find((c) => !used.has(c)) ?? COLORS[Object.keys(S.players).length % COLORS.length];
    const late = S.phase !== 'lobby';
    S.players[peer] = { name, color, alive: !late, spectator: late };
    pos.set(peer, spawn(Object.keys(S.players).length));
    if (late) send(peer, { t: 'chat', from: '', text: 'a game is running: you are watching as a ghost' });
    publish();
  } else if (presence?.host && (!hostId || peer < hostId)) {
    hostId = peer;
    log(`found the host ${presence.name}`);
  }
}

function onPeerLeave(peer, reason) {
  const name = S.players[peer]?.name ?? short(peer);
  log(`${name} left (${reason})`);
  pos.delete(peer);
  if (!isHost && peer === hostId) {
    S = { ...S, phase: 'over', result: { winner: '', why: 'The host left the game.' } };
    render();
    return;
  }
  if (isHost && S.players[peer]) {
    if (S.phase === 'lobby') delete S.players[peer];
    else S.players[peer] = { ...S.players[peer], alive: false, left: true };
    G.votes.delete(peer);
    checkWin();
    publish();
  }
}

// ---------- messages ----------

// Host -> one player (the host itself included).
function send(to, msg, lane = 'reliable') {
  if (to === room.self) return fromHost(msg);
  room.send(msg, { to, lane });
}

function toHost(msg, lane = 'reliable') {
  if (isHost) return hostHandle(room.self, msg);
  if (hostId && room.peers.includes(hostId)) room.send(msg, { to: hostId, lane });
}

// Everyone, the host included.
function broadcast(msg, lane = 'reliable') {
  if (room.peers.length) room.send(msg, { lane });
  fromHost(msg);
}

function publish() {
  const tally = taskTally();
  S.taskTotal = tally.total;
  S.taskDone = tally.done;
  const meeting = S.meeting && { ...S.meeting, ms: Math.max(0, G.meetingEnds - now()), voted: [...G.votes.keys()] };
  broadcast({ t: 'state', s: { ...S, meeting } });
}

function onMessage(msg, from) {
  if (!msg || typeof msg !== 'object') return;
  if (isHost) return hostHandle(from, msg);
  if (!hostId && room.getPresence(from)?.host) hostId = from;
  if (from === hostId) fromHost(msg);
}

// Messages from the host (or, on the host, from itself).
function fromHost(msg) {
  switch (msg.t) {
    case 'state': {
      const s = msg.s;
      if (!s || typeof s !== 'object') return;
      if (!s.players || typeof s.players !== 'object') return;
      if (s.round !== seenRound) {
        const ids = Object.keys(s.players).sort();
        me = spawn(ids.indexOf(room.self) + 1);
        if (isHost) for (const id of ids) pos.set(id, id === room.self ? me : spawn(ids.indexOf(id) + 1));
        pos.set(room.self, me);
      }
      if (s.phase === 'meeting' && seenPhase !== 'meeting') { myVote = null; chatLines.length = 0; renderChat(); }
      if (s.phase === 'lobby' && seenPhase !== 'lobby') { role = null; done.clear(); }
      if (s.meeting) meetingEnds = now() + s.meeting.ms;
      if (s.note && s.note !== seenNote) log(s.note);
      seenRound = s.round; seenPhase = s.phase; seenNote = s.note;
      S = s;
      break;
    }
    case 'snap':
      if (!Array.isArray(msg.p)) return;
      for (const [id, x, y] of msg.p) if (id !== room.self && typeof x === 'number' && typeof y === 'number') pos.set(id, { x, y });
      break;
    case 'role':
      role = { role: msg.role, tasks: msg.tasks ?? [], partners: msg.partners ?? [] };
      done.clear();
      if (msg.role === 'impostor') killReady = now() + FIRST_CD;
      log(msg.role === 'impostor' ? 'you are the IMPOSTOR' : 'you are crew: do your tasks');
      break;
    case 'cd':
      killReady = now() + Number(msg.ms || 0);
      break;
    case 'chat':
      chatLines.push([String(msg.from ?? ''), String(msg.text ?? '').slice(0, 120)]);
      renderChat();
      break;
  }
  render();
}

// ---------- host logic ----------

function hostHandle(from, msg) {
  const p = S.players[from];
  switch (msg.t) {
    case 'pos': {
      const x = Number(msg.x), y = Number(msg.y);
      if (p && Number.isFinite(x) && Number.isFinite(y)) pos.set(from, { x: Math.max(0, Math.min(W, x)), y: Math.max(0, Math.min(H, y)) });
      return;
    }
    case 'chat': {
      const text = String(msg.text ?? '').trim().slice(0, 120);
      if (!p || !text || (S.phase !== 'lobby' && S.phase !== 'meeting') || (S.phase === 'meeting' && !p.alive)) return;
      broadcast({ t: 'chat', from: p.name, text });
      return;
    }
    case 'start':
      if (from === room.self) startGame();
      return;
    case 'again':
      if (from === room.self && S.phase === 'over') toLobby();
      return;
  }
  if (!p || !p.alive || p.spectator) {
    // Ghosts still finish tasks.
    if (msg.t === 'task' && p && !p.spectator && S.phase === 'play') hostTask(from, msg);
    return;
  }
  switch (msg.t) {
    case 'task':
      if (S.phase === 'play') hostTask(from, msg);
      break;
    case 'kill': {
      const t = String(msg.target), victim = S.players[t];
      if (S.phase !== 'play' || !G.impostors.has(from) || !victim?.alive || G.impostors.has(t)) return;
      if (now() < (G.cd.get(from) ?? 0)) return;
      const a = pos.get(from), b = pos.get(t);
      if (!a || !b || dist(a, b) > KILL_RANGE + SLACK) return;
      victim.alive = false;
      S.bodies.push({ id: t, x: b.x, y: b.y, color: victim.color });
      G.cd.set(from, now() + KILL_CD);
      send(from, { t: 'cd', ms: KILL_CD });
      S.note = '';
      checkWin();
      publish();
      break;
    }
    case 'report': {
      const body = S.bodies.find((b) => b.id === String(msg.body));
      const a = pos.get(from);
      if (S.phase !== 'play' || !body || !a || dist(a, body) > REPORT_RANGE + SLACK) return;
      startMeeting(from, `${p.name} found the body of ${S.players[body.id]?.name ?? '?'}`);
      break;
    }
    case 'emergency': {
      const a = pos.get(from);
      if (S.phase !== 'play' || G.used.has(from) || !a || dist(a, BUTTON) > BUTTON_RANGE + SLACK) return;
      G.used.add(from);
      startMeeting(from, `${p.name} called an emergency meeting`);
      break;
    }
    case 'vote': {
      const target = String(msg.target);
      if (S.phase !== 'meeting' || G.votes.has(from) || (target !== 'skip' && !S.players[target]?.alive)) return;
      G.votes.set(from, target);
      const alive = Object.entries(S.players).filter(([, q]) => q.alive).length;
      if (G.votes.size >= alive) endMeeting();
      else publish();
      break;
    }
  }
}

function hostTask(from, msg) {
  const i = Number(msg.i);
  if (G.impostors.has(from) || !G.tasks.get(from)?.includes(i)) return;
  const d = G.done.get(from);
  if (d.has(i)) return;
  d.add(i);
  checkWin();
  publish();
}

function taskTally() {
  let total = 0, n = 0;
  for (const [id, list] of G.tasks) {
    if (S.players[id]?.left) continue;
    total += list.length;
    n += G.done.get(id)?.size ?? 0;
  }
  return { total, done: n };
}

function startGame() {
  const ids = Object.keys(S.players).filter((id) => !S.players[id].spectator);
  if (S.phase !== 'lobby' || ids.length < 2) return;
  const shuffled = [...ids].sort(() => Math.random() - 0.5);
  const nImp = ids.length >= 7 ? 2 : 1;
  G.impostors = new Set(shuffled.slice(0, nImp));
  G.tasks.clear(); G.done.clear(); G.cd.clear(); G.votes.clear(); G.used.clear();
  for (const id of ids) {
    const tasks = [...TASKS.keys()].sort(() => Math.random() - 0.5).slice(0, TASKS_EACH);
    const imp = G.impostors.has(id);
    if (!imp) { G.tasks.set(id, tasks); G.done.set(id, new Set()); }
    G.cd.set(id, now() + FIRST_CD);
    S.players[id].alive = true;
    send(id, { t: 'role', role: imp ? 'impostor' : 'crew', tasks, partners: imp ? [...G.impostors].filter((x) => x !== id) : [] });
  }
  S = { ...S, phase: 'play', round: S.round + 1, bodies: [], meeting: null, result: null, note: `the game started: ${nImp} impostor${nImp > 1 ? 's' : ''} among ${ids.length}` };
  publish();
}

function startMeeting(caller, why) {
  G.votes.clear();
  G.meetingEnds = now() + MEETING_MS;
  S = { ...S, phase: 'meeting', meeting: { caller, why }, note: why };
  publish();
}

function endMeeting() {
  if (S.phase !== 'meeting') return;
  const count = new Map();
  for (const t of G.votes.values()) count.set(t, (count.get(t) ?? 0) + 1);
  const ranked = [...count].sort((a, b) => b[1] - a[1]);
  let note = 'No one was ejected.';
  if (ranked.length && ranked[0][0] !== 'skip' && (ranked.length === 1 || ranked[0][1] > ranked[1][1])) {
    const out = S.players[ranked[0][0]];
    out.alive = false;
    note = `${out.name} was ejected. ${out.name} was ${G.impostors.has(ranked[0][0]) ? '' : 'not '}an impostor.`;
  }
  S = { ...S, phase: 'play', round: S.round + 1, bodies: [], meeting: null, note };
  for (const id of G.impostors) { G.cd.set(id, now() + KILL_CD); send(id, { t: 'cd', ms: KILL_CD }); }
  checkWin();
  publish();
}

function checkWin() {
  if (S.phase !== 'play' && S.phase !== 'meeting') return;
  const live = Object.entries(S.players).filter(([, p]) => p.alive && !p.spectator && !p.left);
  const imps = live.filter(([id]) => G.impostors.has(id)).length;
  const crew = live.length - imps;
  const { total, done: n } = taskTally();
  let winner = null, why = '';
  if (imps === 0) { winner = 'crew'; why = 'Every impostor is out.'; }
  else if (imps >= crew) { winner = 'impostor'; why = 'The impostors equal the crew.'; }
  else if (total > 0 && n >= total) { winner = 'crew'; why = 'Every task is done.'; }
  if (!winner) return;
  const names = [...G.impostors].map((id) => S.players[id]?.name ?? '?');
  S = { ...S, phase: 'over', meeting: null, result: { winner, why, impostors: names } };
}

function toLobby() {
  for (const id of Object.keys(S.players)) {
    if (S.players[id].left) delete S.players[id];
    else S.players[id] = { ...S.players[id], alive: true, spectator: false };
  }
  G.impostors.clear(); G.tasks.clear(); G.done.clear();
  S = { ...S, phase: 'lobby', round: S.round + 1, bodies: [], meeting: null, result: null, note: 'back in the waiting room' };
  publish();
}

// The host's 15 Hz snapshot and meeting clock.
setInterval(() => {
  if (!isHost || !room) return;
  if (S.phase === 'meeting' && now() > G.meetingEnds) endMeeting();
  const p = [...pos].filter(([id]) => S.players[id]).map(([id, q]) => [id, Math.round(q.x), Math.round(q.y)]);
  if (room.peers.length) room.send({ t: 'snap', p }, { lane: 'unreliable' });
}, 66);

// ---------- the player's side ----------

const myRole = () => role?.role ?? 'crew';
const alive = () => S.players[room?.self]?.alive ?? false;
const canMove = () => room && (S.phase === 'lobby' || S.phase === 'play');

function nearestTask() {
  if (!role || S.phase !== 'play') return -1;
  let best = -1, bd = USE_RANGE;
  for (const i of role.tasks) {
    if (done.has(i)) continue;
    const d = dist(me, { x: TASKS[i][1], y: TASKS[i][2] });
    if (d <= bd) { bd = d; best = i; }
  }
  return best;
}

function killTarget() {
  if (myRole() !== 'impostor' || !alive() || S.phase !== 'play' || now() < killReady) return null;
  let best = null, bd = KILL_RANGE;
  for (const [id, p] of Object.entries(S.players)) {
    const q = pos.get(id);
    if (id === room.self || !p.alive || role.partners.includes(id) || !q) continue;
    const d = dist(me, q);
    if (d <= bd) { bd = d; best = id; }
  }
  return best;
}

function nearBody() {
  if (!alive() || S.phase !== 'play') return null;
  return S.bodies.find((b) => dist(me, b) <= REPORT_RANGE) ?? null;
}

function use() {
  if (S.phase !== 'play') return;
  if (alive() && dist(me, BUTTON) <= BUTTON_RANGE) return toHost({ t: 'emergency' });
  holding = true;
}
function kill() { const t = killTarget(); if (t) toHost({ t: 'kill', target: t }); }
function report() { const b = nearBody(); if (b) toHost({ t: 'report', body: b.id }); }

let last = now();
let lastSent = 0;
function frame() {
  const t = now();
  const dt = Math.min(0.05, (t - last) / 1000);
  last = t;
  if (canMove()) {
    let dx = 0, dy = 0;
    if (keys.has('ArrowLeft') || keys.has('KeyA')) dx--;
    if (keys.has('ArrowRight') || keys.has('KeyD')) dx++;
    if (keys.has('ArrowUp') || keys.has('KeyW')) dy--;
    if (keys.has('ArrowDown') || keys.has('KeyS')) dy++;
    if (dx || dy) {
      const k = (SPEED * dt) / Math.hypot(dx, dy);
      if (walkable(me.x + dx * k, me.y)) me.x += dx * k;
      if (walkable(me.x, me.y + dy * k)) me.y += dy * k;
    }
    if (t - lastSent > 50) {
      lastSent = t;
      if (isHost) pos.set(room.self, me);
      else toHost({ t: 'pos', x: Math.round(me.x), y: Math.round(me.y) }, 'unreliable');
    }
  }
  const task = holding ? nearestTask() : -1;
  if (task >= 0 && task === holdFor) {
    holdMs += dt * 1000;
    if (holdMs >= TASK_MS) {
      done.add(task);
      toHost({ t: 'task', i: task });
      log(`done: ${TASKS[task][0]}`);
      holdMs = 0; holdFor = -1;
    }
  } else { holdFor = task; holdMs = 0; }
  draw();
  requestAnimationFrame(frame);
}

// ---------- drawing ----------

function draw() {
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.fillStyle = '#05060a';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  if (!room) return;
  const cam = { x: me.x - canvas.width / 2, y: me.y - canvas.height / 2 };
  ctx.translate(-cam.x, -cam.y);
  ctx.fillStyle = '#191d29';
  for (const [x, y, w, h] of HALLS) ctx.fillRect(x, y, w, h);
  ctx.font = '600 15px system-ui';
  for (const [name, x, y, w, h] of ROOMS) {
    ctx.fillStyle = '#1f2433';
    ctx.fillRect(x, y, w, h);
    ctx.fillStyle = '#3a4052';
    ctx.fillText(name.toUpperCase(), x + 12, y + 24);
  }
  ctx.fillStyle = '#7a1f22';
  ctx.beginPath(); ctx.arc(BUTTON.x, BUTTON.y, 18, 0, 7); ctx.fill();
  ctx.fillStyle = '#e5484d';
  ctx.beginPath(); ctx.arc(BUTTON.x, BUTTON.y, 12, 0, 7); ctx.fill();
  if (role && S.phase === 'play') {
    for (const i of role.tasks) {
      const [name, x, y] = TASKS[i];
      const fin = done.has(i);
      ctx.strokeStyle = fin ? '#46a758' : '#ffe629';
      ctx.lineWidth = 3;
      ctx.beginPath(); ctx.arc(x, y, 14, 0, 7); ctx.stroke();
      ctx.fillStyle = fin ? '#46a758' : '#ffe629';
      ctx.font = '12px system-ui';
      ctx.fillText(name + (role.role === 'impostor' ? ' (fake)' : ''), x - 30, y - 20);
      if (holdFor === i && holdMs > 0) {
        ctx.beginPath(); ctx.moveTo(x, y); ctx.arc(x, y, 10, -Math.PI / 2, -Math.PI / 2 + (holdMs / TASK_MS) * Math.PI * 2); ctx.fill();
      }
    }
  }
  const ghost = !alive() && S.phase === 'play';
  const vis = ghost || S.phase !== 'play' ? Infinity : VISION[myRole()] ?? VISION.crew;
  for (const b of S.bodies) {
    if (dist(me, b) > vis) continue;
    ctx.fillStyle = b.color;
    ctx.beginPath(); ctx.ellipse(b.x, b.y + 6, R, R * 0.55, 0, 0, 7); ctx.fill();
    ctx.fillStyle = '#f5f5f5';
    ctx.fillRect(b.x - 2, b.y - 8, 4, 10);
  }
  for (const [id, p] of Object.entries(S.players)) {
    const q = id === room.self ? me : pos.get(id);
    if (!q || p.left) continue;
    const dead = !p.alive && (S.phase === 'play' || S.phase === 'meeting');
    if (dead && !ghost && id !== room.self) continue; // the living don't see ghosts
    if (id !== room.self && dist(me, q) > vis) continue;
    ctx.globalAlpha = dead ? 0.4 : 1;
    ctx.fillStyle = p.color;
    ctx.beginPath(); ctx.arc(q.x, q.y, R, 0, 7); ctx.fill();
    ctx.fillStyle = '#9fd3ff';
    ctx.beginPath(); ctx.ellipse(q.x + 6, q.y - 4, 7, 5, 0, 0, 7); ctx.fill();
    const imp = role?.role === 'impostor' && (id === room.self || role.partners.includes(id));
    ctx.fillStyle = imp ? '#ff6b6b' : '#e6e8ee';
    ctx.font = '12px system-ui';
    ctx.textAlign = 'center';
    ctx.fillText(p.name, q.x, q.y - R - 6);
    ctx.textAlign = 'start';
    ctx.globalAlpha = 1;
  }
  if (Number.isFinite(vis)) {
    ctx.fillStyle = 'rgba(0, 0, 0, 0.93)';
    ctx.beginPath();
    ctx.rect(cam.x, cam.y, canvas.width, canvas.height);
    ctx.arc(me.x, me.y, vis, 0, Math.PI * 2, true);
    ctx.fill();
  }
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  hud();
}

function hud() {
  let html = '';
  if (S.phase === 'play' && role) {
    const r = role.role === 'impostor' ? '<b style="color:#ff6b6b">IMPOSTOR</b>' : '<b style="color:#9fd3ff">CREW</b>';
    html += `${r}${alive() ? '' : ' <span class="dim">(ghost)</span>'}`;
    if (role.role === 'impostor' && role.partners.length) html += ` with ${role.partners.map((id) => esc(S.players[id]?.name ?? '?')).join(', ')}`;
    if (role.role === 'impostor' && alive()) {
      const cd = Math.ceil((killReady - now()) / 1000);
      html += cd > 0 ? ` · kill in ${cd}s` : ' · kill ready';
    }
    html += `<div>tasks ${S.taskDone}/${S.taskTotal}</div><div id="bar"><div style="width:${S.taskTotal ? (100 * S.taskDone) / S.taskTotal : 0}%"></div></div>`;
  } else if (S.phase === 'lobby') html = 'waiting room: walk around';
  $('hud').innerHTML = html;
  const playing = S.phase === 'play' && role && alive();
  $('actions').classList.toggle('hidden', !(S.phase === 'play' && role));
  $('kill').classList.toggle('hidden', role?.role !== 'impostor');
  $('kill').disabled = !killTarget();
  $('report').disabled = !playing || !nearBody();
  $('use').disabled = !(nearestTask() >= 0 || (playing && dist(me, BUTTON) <= BUTTON_RANGE));
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

// The overlays: waiting room, meeting, game over.
function render() {
  const ph = S.phase;
  const finding = ph === 'finding';
  $('wait').classList.toggle('hidden', !finding && ph !== 'lobby');
  if (finding) {
    $('waittitle').textContent = 'Finding the host…';
    $('waitmsg').innerHTML = findingMsg();
    $('roster').innerHTML = '';
  }
  // The lobby is played on the map, under a see-through card.
  const lobby = ph === 'lobby';
  $('wait').style.background = lobby ? 'transparent' : '';
  $('wait').style.placeItems = lobby ? 'start center' : '';
  $('wait').style.pointerEvents = lobby ? 'none' : '';
  $('start').style.pointerEvents = 'auto';
  $('start').classList.toggle('hidden', !(lobby && isHost));
  if (lobby) {
    $('waittitle').textContent = `Waiting room (${Object.keys(S.players).length})`;
    $('waitmsg').innerHTML = isHost ? 'Share the room code. Start with 4+ players for a real game (2 work for a test).' : 'Waiting for the host to start.';
    $('roster').innerHTML = Object.values(S.players).map((p) => `<li><span class="dot" style="background:${p.color}"></span>${esc(p.name)}</li>`).join('');
    $('start').disabled = Object.keys(S.players).length < 2;
  }
  $('meeting').classList.toggle('hidden', ph !== 'meeting');
  if (ph === 'meeting' && S.meeting) {
    $('meettitle').textContent = 'Emergency meeting';
    $('meetmsg').textContent = `${S.meeting.why}. ${Math.max(0, Math.ceil((meetingEnds - now()) / 1000))}s left. ${alive() ? (myVote ? 'You voted.' : 'Who is the impostor?') : 'You are dead: watch.'}`;
    const voted = new Set(S.meeting.voted);
    const rows = Object.entries(S.players).filter(([, p]) => !p.spectator && !p.left).map(([id, p]) =>
      `<div class="vote"><span><span class="dot" style="background:${p.color}"></span>${esc(p.name)}${p.alive ? '' : ' <span class="dim">(dead)</span>'}${voted.has(id) ? ' <span class="dim">✓ voted</span>' : ''}</span>` +
      (p.alive && alive() && !myVote ? `<button data-vote="${id}">vote</button>` : '') + '</div>');
    rows.push(`<div class="vote"><span class="dim">skip</span>${alive() && !myVote ? '<button data-vote="skip">skip</button>' : ''}</div>`);
    $('votes').innerHTML = rows.join('');
  }
  $('over').classList.toggle('hidden', ph !== 'over');
  if (ph === 'over' && S.result) {
    const r = S.result;
    $('overtitle').textContent = r.winner === 'crew' ? 'Crew wins' : r.winner === 'impostor' ? 'Impostor wins' : 'Game over';
    $('overmsg').textContent = `${r.why}${r.impostors ? ` The impostor${r.impostors.length > 1 ? 's were' : ' was'} ${r.impostors.join(', ')}.` : ''}`;
    $('again').classList.toggle('hidden', !isHost);
  }
}

function findingMsg() {
  const secs = Math.round((now() - started) / 1000);
  const relays = room.signaling.stats ? `${room.signaling.stats.relaysOpen} signalling relay(s) open. ` : '';
  const heard = [...seen.keys()];
  const linked = heard.filter((p) => room.peers.includes(p)).length;
  let msg = `${relays}${secs}s: heard ${heard.length} peer(s) in this room, linked to ${linked}.`;
  if (secs > 15 && heard.length > linked) {
    msg += '<br><br><b>Found peers but could not connect.</b> WebRTC failed (both networks block direct connections: strict, "symmetric" NAT), so the fallback through another player or the signalling relays is being tried. If this stays, the host may run an older copy of the game.';
  } else if (secs > 15 && heard.length === 0) {
    msg += '<br><br>No one with this room code yet. Check the code; the host must click "Host a game" first.';
  }
  return msg;
}

function renderChat() {
  $('chatlog').innerHTML = chatLines.map(([from, text]) => `<div>${from ? `<b>${esc(from)}</b>: ` : ''}${esc(text)}</div>`).join('');
  $('chatlog').scrollTop = 1e6;
}

// The network panel: who is linked, how (ICE candidate types), and RTT.
async function pathOf(peer) {
  const link = [...(room.transport.links?.values() ?? [])].find((l) => l.peer === peer);
  if (!link?.pc) return '';
  try {
    const stats = await link.pc.getStats();
    let pair = null;
    stats.forEach((r) => { if (r.type === 'transport' && r.selectedCandidatePairId) pair = stats.get(r.selectedCandidatePairId); });
    if (!pair) stats.forEach((r) => { if (r.type === 'candidate-pair' && r.nominated && r.state === 'succeeded') pair = r; });
    if (!pair) return link.pc.iceConnectionState;
    const l = stats.get(pair.localCandidateId), r = stats.get(pair.remoteCandidateId);
    const fam = String(l?.address ?? l?.ip ?? '').includes(':') ? 'IPv6' : 'IPv4';
    const relayed = l?.candidateType === 'relay' || r?.candidateType === 'relay';
    return `${relayed ? 'TURN relay' : 'direct'} ${fam} (${l?.candidateType}↔${r?.candidateType})`;
  } catch { return ''; }
}

// The fallback routes, for peers WebRTC couldn't link.
async function routeOf(peer) {
  const r = room.route(peer);
  if (r?.kind === 'peer') return `relayed by ${esc(room.getPresence(r.via)?.name ?? short(r.via))} (WebRTC failed)`;
  if (r?.kind === 'signaling') return 'over the signalling relays (WebRTC failed; slower)';
  return pathOf(peer);
}

setInterval(async () => {
  if (!room) return;
  if (S.phase === 'finding' || S.phase === 'meeting') render();
  const rows = [];
  for (const peer of room.peers) {
    const name = room.getPresence(peer)?.name ?? short(peer);
    const rtt = room.rtt(peer);
    const tag = peer === hostId ? ' <b>host</b>' : '';
    rows.push(`<li>${esc(name)}${tag}: ${rtt == null ? '…' : `${Math.round(rtt)} ms`} <span class="dim">${await routeOf(peer)}</span></li>`);
  }
  for (const [peer, t] of seen) {
    if (room.peers.includes(peer)) continue;
    rows.push(`<li class="dim">${short(peer)}: heard via signalling ${Math.round((now() - t) / 1000)}s ago, not linked</li>`);
  }
  const relays = room.signaling.stats ? `<div>signalling relays open: ${room.signaling.stats.relaysOpen}</div>` : '';
  $('net').innerHTML = `${relays}<ul>${rows.join('') || '<li>no peers yet</li>'}</ul>${params.get('turn') ? '<div>TURN configured</div>' : ''}`;
}, 1000);

// ---------- input ----------

addEventListener('keydown', (e) => {
  if (e.target instanceof HTMLInputElement) return;
  keys.add(e.code);
  if (e.code === 'KeyE' && !e.repeat) use();
  if (e.code === 'KeyQ') kill();
  if (e.code === 'KeyR') report();
});
addEventListener('keyup', (e) => {
  keys.delete(e.code);
  if (e.code === 'KeyE') holding = false;
});
addEventListener('blur', () => { keys.clear(); holding = false; });
$('use').onpointerdown = use;
$('use').onpointerup = $('use').onpointerleave = () => (holding = false);
$('kill').onclick = kill;
$('report').onclick = report;
$('start').onclick = () => toHost({ t: 'start' });
$('again').onclick = () => toHost({ t: 'again' });
$('votes').onclick = (e) => {
  const v = e.target.dataset?.vote;
  if (!v || myVote) return;
  myVote = v;
  toHost({ t: 'vote', target: v });
  render();
};
$('chat').onsubmit = (e) => {
  e.preventDefault();
  const text = $('say').value.trim();
  if (text) toHost({ t: 'chat', text });
  $('chat').reset();
};
$('host').onclick = () => enter(true);
$('join').onclick = () => enter(false);
$('random').onclick = () => {
  const a = new Uint8Array(6);
  crypto.getRandomValues(a);
  $('code').value = [...a].map((b) => 'abcdefghjkmnpqrstuvwxyz23456789'[b % 31]).join('');
};
$('name').value = params.get('name') ?? localStorage.getItem('crew-name') ?? '';
$('code').value = params.get('room') ?? '';
if (params.get('auto') === 'host') enter(true);
else if (params.get('auto') === 'join') enter(false);

// For tests: read-only views and the actions a player takes.
window.crew = {
  get phase() { return S.phase; }, get state() { return S; }, get role() { return role; }, get me() { return me; },
  get self() { return room?.self; }, get hostId() { return hostId; }, pos, done,
  routes: () => Object.fromEntries((room?.peers ?? []).map((p) => [room.getPresence(p)?.name, room.route(p)?.kind])),
  netStats: () => room && ({ rtt: room.peers.map((p) => room.rtt(p)), core: room.stats(), piped: room.transport.piped, signaling: room.signaling.stats }),
  goto(x, y) { me.x = x; me.y = y; }, use, kill, report, killTarget, nearestTask,
  vote(t) { myVote = t; toHost({ t: 'vote', target: t }); }, start: () => toHost({ t: 'start' }), say: (text) => toHost({ t: 'chat', text }),
  finishTask(i) { done.add(i); toHost({ t: 'task', i }); },
};

requestAnimationFrame(frame);
