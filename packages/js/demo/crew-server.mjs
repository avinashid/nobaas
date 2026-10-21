// Serves the JS package (demo/crew.html and what it imports) for hosting the
// Crew demo somewhere other than this machine. It is only a file server: players
// link peer to peer, and when WebRTC can't link two of them, joinRoom falls back
// on its own (through another player, else over the Nostr relays). No dependencies.
//
//   node packages/js/demo/crew-server.mjs [port=8000] [host=127.0.0.1]

import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const port = Number(process.argv[2] ?? 8000);
const host = process.argv[3] ?? '127.0.0.1';
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');

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

server.listen(port, host, () => console.log(`crew: http://${host}:${port}/`));
