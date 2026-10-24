// Where loadPeerwire reads the module from, in Node: an absolute path or a file: URL (string or
// URL) from disk, an http: URL by fetch, bytes as they are. A missing file fails with the disk's
// ENOENT, not a fetch error, and an HTTP error status rejects with it. test/wasm_url.mjs checks
// the browser side, where a root-relative path is fetched.
//
//   node packages/js/test/wasm_source.mjs
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { pathToFileURL } from 'node:url';
import { loadPeerwire } from '../src/peerwire.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const loads = async (source, what) => {
  try {
    const ep = (await loadPeerwire(source)).endpoint();
    check(/^[0-9a-f]{64}$/.test(ep.id), what);
    ep.close();
  } catch (e) {
    check(false, `${what}: ${e.message}`);
  }
};
const rejects = async (source, pattern, what) => {
  const e = await loadPeerwire(source).then(() => null, (e) => e);
  check(e && pattern.test(`${e.code ?? ''} ${e.message}`), `${what}${e ? `: ${e.code ?? ''} ${e.message}` : ': resolved'}`);
};

const url = new URL('../pkg/peerwire_ffi.wasm', import.meta.url);
const path = url.pathname;
const bytes = await readFile(url);
const missing = `${path}.missing-${process.pid}`;

await loads(path, 'an absolute path reads from disk');
await loads(url, 'a file: URL object reads from disk');
await loads(pathToFileURL(path).href, 'a file: URL string reads from disk');
await loads(bytes, 'bytes load as they are');
await rejects(missing, /ENOENT/, 'a missing absolute path fails on disk, not in fetch');

const served = [];
const server = createServer((req, res) => {
  served.push(req.url);
  if (req.url !== '/pkg/peerwire_ffi.wasm') return res.writeHead(404).end('not found');
  res.writeHead(200, { 'content-type': 'application/wasm' }).end(bytes);
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
try {
  await loads(`${base}/pkg/peerwire_ffi.wasm`, 'an http: URL is fetched');
  check(served.includes('/pkg/peerwire_ffi.wasm'), 'the server saw the fetch');
  await rejects(`${base}/nope.wasm`, /HTTP 404/, 'a 404 rejects with its status');
} finally {
  server.close();
}

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
