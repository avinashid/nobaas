import { joinRoom } from '../src/index.js';

const params = new URLSearchParams(location.search);
const room = await joinRoom({ appId: 'nobaas-demo', roomId: `poll/${params.get('room') ?? 'demo'}`, queue: true });
const hue = parseInt(room.self.slice(0, 4), 16) % 360;
const me = { x: 200 + Math.random() * 880, y: 150 + Math.random() * 420, hue };
const others = new Map(); // peer id -> { x, y, hue }
const keys = new Set();
let events = 0;
let t = 0;
addEventListener('keydown', (e) => keys.add(e.key));
addEventListener('keyup', (e) => keys.delete(e.key));
document.getElementById('me').textContent = room.self.slice(0, 8);

const ctx = document.getElementById('view').getContext('2d');
const dot = (p, label, mine) => {
  ctx.fillStyle = `hsl(${p.hue} 80% 62%)`;
  ctx.beginPath();
  ctx.arc(p.x, p.y, mine ? 26 : 22, 0, Math.PI * 2);
  ctx.fill();
  if (mine) {
    ctx.strokeStyle = '#fff';
    ctx.lineWidth = 3;
    ctx.stroke();
  }
  ctx.fillStyle = '#e6e8ee';
  ctx.fillText(label, p.x, p.y + 50);
};

function frame() {
  // Everything from the network since the last frame, as plain values.
  for (const e of room.poll()) {
    events++;
    if (e.type === 'peer-leave') others.delete(e.peer);
    else if (e.type === 'message' && e.kind === 'text') {
      const [x, y, h] = e.data.split(',').map(Number);
      others.set(e.peer, { x, y, hue: h });
    }
  }
  t += 1 / 60;
  const v = 7;
  if (keys.has('ArrowLeft')) me.x -= v;
  if (keys.has('ArrowRight')) me.x += v;
  if (keys.has('ArrowUp')) me.y -= v;
  if (keys.has('ArrowDown')) me.y += v;
  if (!keys.size) me.x += Math.cos(t * 1.3 + hue) * 2, me.y += Math.sin(t * 1.7 + hue) * 2;
  me.x = Math.max(30, Math.min(1250, me.x));
  me.y = Math.max(30, Math.min(690, me.y));
  room.sendTo('', `${me.x.toFixed(1)},${me.y.toFixed(1)},${hue}`, 'unreliable');

  ctx.clearRect(0, 0, 1280, 720);
  ctx.font = '22px system-ui, sans-serif';
  ctx.textAlign = 'center';
  for (const [id, p] of others) dot(p, id.slice(0, 6), false);
  dot(me, 'you', true);
  document.getElementById('peers').textContent = room.peers.length ? `${room.peers.length} peer${room.peers.length > 1 ? 's' : ''}` : 'alone';
  document.getElementById('events').textContent = `${events} events`;
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);

// Hooks for automated tests.
window.demo = { id: room.self, seen: () => [...others.keys()], events: () => events };
