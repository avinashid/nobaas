// Serves the JS package (demo/crew.html and what it imports) and, at /relay, a
// WebSocket fallback for demo/crew.js: when two players can't link by WebRTC
// (both behind symmetric NAT and no TURN), their game messages go through this
// server instead. It works wherever the page itself loads, as it is the same
// origin. No dependencies.
//
//   node packages/js/demo/crew-server.mjs [port=8000] [host=127.0.0.1]
//
// Relay protocol, JSON text frames. The client's first frame is
//   {t:'hello', room:<64 hex>, id:<its peer id>, presence:{...}}
// and the server answers {t:'join', id, presence} for each peer already in the
// room (and later joiners), {t:'leave', id} when one goes. Then
//   {t:'to', to:[ids], m:<message>} -> {t:'msg', from, m} to each of those ids.
// Ids are claimed, not proved: anyone with the room key can join as any unused id.

import http from 'node:http';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const port = Number(process.argv[2] ?? 8000);
const host = process.argv[3] ?? '127.0.0.1';
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');

const MAX_FRAME = 60 * 1024; // an incoming frame; what goes out must stay under 64 KiB
const MAX_ROOM = 16;
const MAX_SOCKETS = 400;
const RATE = 120; // frames per second per socket, burst twice that
const BYTES = 256 * 1024; // bytes per second per socket, burst twice that
const PING_MS = 25000;

const TYPES = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.css': 'text/css', '.json': 'application/json', '.png': 'image/png',
};

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname === '/') {
    res.writeHead(302, { location: '/demo/crew.html' });
    return res.end();
  }
  const rel = path.normalize(decodeURIComponent(url.pathname));
  const file = path.join(root, rel);
  const type = TYPES[path.extname(file)];
  if (req.method !== 'GET' || !file.startsWith(root + path.sep) || !type || rel.includes(`${path.sep}.`)) {
    res.writeHead(404);
    return res.end();
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': type, 'cache-control': 'no-cache' });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
});

// ---- WebSocket (RFC 6455), text frames only ----

const rooms = new Map(); // room key -> Map(id -> client)
let sockets = 0;

server.on('upgrade', (req, sock) => {
  const key = req.headers['sec-websocket-key'];
  if (new URL(req.url, 'http://x').pathname !== '/relay' || !key || sockets >= MAX_SOCKETS) return sock.destroy();
  const accept = createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  sock.setNoDelay(true);
  sockets++;
  const c = { sock, buf: Buffer.alloc(0), room: null, id: null, frames: RATE * 2, bytes: BYTES * 2, at: Date.now() };
  const ping = setInterval(() => frame(c, 0x9, Buffer.alloc(0)), PING_MS);
  sock.on('data', (d) => {
    c.buf = Buffer.concat([c.buf, d]);
    if (c.buf.length > MAX_FRAME * 4) return sock.destroy();
    let f;
    while ((f = parse(c))) {
      if (f.op === 0x8) return sock.end();
      if (f.op === 0x9) frame(c, 0xa, f.data);
      if (f.op === 0x1 && f.fin) onText(c, f.data.toString('utf8'));
      else if (f.op === 0x0 || (f.op === 0x1 && !f.fin) || f.op === 0x2) return sock.destroy(); // no fragments, no binary
    }
  });
  sock.on('error', () => {});
  sock.on('close', () => {
    clearInterval(ping);
    sockets--;
    leave(c);
  });
});

function parse(c) {
  const b = c.buf;
  if (b.length < 2) return null;
  const fin = (b[0] & 0x80) !== 0, op = b[0] & 0x0f, masked = (b[1] & 0x80) !== 0;
  let len = b[1] & 0x7f, at = 2;
  if (len === 126) {
    if (b.length < 4) return null;
    len = b.readUInt16BE(2);
    at = 4;
  } else if (len === 127) {
    c.sock.destroy();
    return null;
  }
  if (!masked || len > MAX_FRAME) {
    c.sock.destroy();
    return null;
  }
  if (b.length < at + 4 + len) return null;
  const mask = b.subarray(at, at + 4);
  const data = Buffer.from(b.subarray(at + 4, at + 4 + len));
  for (let i = 0; i < data.length; i++) data[i] ^= mask[i & 3];
  c.buf = b.subarray(at + 4 + len);
  return { fin, op, data };
}

function frame(c, op, data) {
  if (c.sock.destroyed) return;
  const n = data.length;
  const head = n < 126 ? Buffer.from([0x80 | op, n]) : n < 65536 ? Buffer.from([0x80 | op, 126, n >> 8, n & 255]) : null;
  if (!head) return;
  c.sock.write(Buffer.concat([head, data]));
}

const sendJson = (c, obj) => frame(c, 0x1, Buffer.from(JSON.stringify(obj)));

function onText(c, text) {
  const t = Date.now();
  c.frames = Math.min(RATE * 2, c.frames + ((t - c.at) * RATE) / 1000);
  c.bytes = Math.min(BYTES * 2, c.bytes + ((t - c.at) * BYTES) / 1000);
  c.at = t;
  if (c.frames < 1 || c.bytes < text.length) return; // over the rate: dropped
  c.frames -= 1;
  c.bytes -= text.length;
  let m;
  try { m = JSON.parse(text); } catch { return c.sock.destroy(); }
  if (!m || typeof m !== 'object') return;
  if (m.t === 'hello' && !c.room) {
    const ok = typeof m.room === 'string' && /^[0-9a-f]{64}$/.test(m.room) && typeof m.id === 'string' && m.id.length > 0 && m.id.length <= 128;
    const presence = m.presence && typeof m.presence === 'object' ? m.presence : {};
    if (!ok || JSON.stringify(presence).length > 1024) return c.sock.destroy();
    const members = rooms.get(m.room) ?? new Map();
    if (members.has(m.id) || members.size >= MAX_ROOM) return sendJson(c, { t: 'refused', why: members.has(m.id) ? 'id in use' : 'room full' });
    for (const [id, o] of members) {
      sendJson(c, { t: 'join', id, presence: o.presence });
      sendJson(o, { t: 'join', id: m.id, presence });
    }
    Object.assign(c, { room: m.room, id: m.id, presence });
    members.set(m.id, c);
    rooms.set(m.room, members);
  } else if (m.t === 'to' && c.room && Array.isArray(m.to) && m.to.length <= MAX_ROOM) {
    const members = rooms.get(c.room);
    const out = Buffer.from(JSON.stringify({ t: 'msg', from: c.id, m: m.m }));
    for (const id of new Set(m.to)) {
      const o = members.get(id);
      if (o && o !== c) frame(o, 0x1, out);
    }
  }
}

function leave(c) {
  const members = c.room && rooms.get(c.room);
  if (!members || members.get(c.id) !== c) return;
  members.delete(c.id);
  if (!members.size) rooms.delete(c.room);
  for (const o of members.values()) sendJson(o, { t: 'leave', id: c.id });
}

server.listen(port, host, () => console.log(`crew: http://${host}:${port}/  (relay at /relay)`));
