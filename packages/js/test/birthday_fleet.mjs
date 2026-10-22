// NB-048: the live browser birthday-punch fleet (src/birthday.js openFleet/openSpray)
// in headless Chromium, on loopback. Loopback has no NAT filtering, so to isolate the
// real mechanism the two sides advertise NO candidates: the hard side opens a fleet of
// connections sharing one certificate and one set of ICE credentials, and the easy
// side reaches them ONLY through its sprayed guesses (as a real cone only lets in a
// port it has sprayed). The test seeds a few of the fleet's real socket ports into the
// spray (as a guess that lands on a NAT mapping would), and checks:
// - the fleet answers with ONE shared answer (one ufrag/pwd, one fingerprint);
// - exactly one connection's data channel opens, datagrams flow both ways on it;
// - the other connections are closed;
// - with no real port in the spray, nothing opens (the guesses must hit).
// The NAT maths (that a guess hits one of the mappings) is the native simulator's job
// (NB-044); this proves the browser can drive the fleet.
//
//   node packages/js/test/birthday_fleet.mjs [fleet=32]
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const fleet = Number(process.argv[2] ?? 32); // fewer than 256 for a fast loopback run; the mechanism is the same
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  page.on('pageerror', (e) => ok(false, `page error: ${e.message}`));
  await page.goto(`http://127.0.0.1:${port}/`);
  const out = await page.evaluate(async (fleet) => {
    const { openFleet, openSpray, randomCredentials, sprayPorts } = await import('/src/birthday.js');
    const res = [];
    const ok = (cond, what) => res.push([!!cond, what]);
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    const ufrag = (sdp) => /a=ice-ufrag:(\S+)/.exec(sdp)?.[1];
    const fp = (sdp) => /a=fingerprint:\S+ (\S+)/.exec(sdp)?.[1];
    const until = async (cond, ms = 10000) => { const t = performance.now(); while (!cond() && performance.now() - t < ms) await sleep(50); return cond(); };

    // The hit case: a few of the fleet's real socket ports are among the sprayed guesses.
    {
      const cert = await RTCPeerConnection.generateCertificate({ name: 'ECDSA', namedCurve: 'P-256' });
      const creds = randomCredentials();
      const easy = await openSpray({ RTCPeerConnection });
      const hard = await openFleet({ RTCPeerConnection, cert, creds, offerSdp: easy.offerSdp, size: fleet });
      // Every connection in the fleet answered with the one shared ufrag and one fingerprint.
      const sdps = hard.conns.map((c) => c.pc.localDescription.sdp);
      ok(new Set(sdps.map(ufrag)).size === 1 && new Set(sdps.map(fp)).size === 1 && ufrag(hard.answerSdp) === ufrag(sdps[0]), `fleet of ${fleet} shares one ufrag and one fingerprint`);
      ok(!/a=candidate:/.test(hard.answerSdp) && !/a=candidate:/.test(easy.offerSdp), 'neither side advertises a candidate (only the spray connects)');
      // Wait for the fleet to gather its socket ports, then spray decoys plus a few real ones.
      await until(() => hard.ports().length >= Math.min(3, fleet));
      const real = hard.ports().slice(0, 3);
      const spray = [...sprayPorts(60), ...real];
      const t0 = performance.now();
      await easy.applyAnswer(hard.answerSdp, '127.0.0.1', spray);
      const winner = await Promise.race([hard.done, sleep(15000).then(() => null)]);
      ok(winner, `a connection won (its channel opened) in ${Math.round(performance.now() - t0)} ms`);
      const easyUp = await until(() => easy.dc.readyState === 'open', 4000);
      ok(easyUp, 'the easy side channel is open too');
      // Datagrams both ways on the winning channel.
      if (winner && easyUp) {
        const got = { easy: null, hard: null };
        easy.dc.onmessage = (e) => (got.easy = new Uint8Array(e.data)[0]);
        winner.dc.onmessage = (e) => (got.hard = new Uint8Array(e.data)[0]);
        easy.dc.send(Uint8Array.of(0xee));
        winner.dc.send(Uint8Array.of(0xaa));
        await until(() => got.easy === 0xaa && got.hard === 0xee, 3000);
        ok(got.easy === 0xaa && got.hard === 0xee, 'datagrams flow both ways on the winning channel');
      }
      // The losers are closed.
      await until(() => hard.conns.filter((c) => c.pc.connectionState !== 'closed').length <= 1, 3000);
      const openCount = hard.conns.filter((c) => c.pc.connectionState !== 'closed').length;
      ok(openCount === 1, `only the winner stays open (${openCount} of ${fleet})`);
      easy.pc.close(); hard.conns.forEach((c) => c.pc.close());
    }

    // The miss case: no real port in the spray → nothing opens.
    {
      const cert = await RTCPeerConnection.generateCertificate({ name: 'ECDSA', namedCurve: 'P-256' });
      const creds = randomCredentials();
      const easy = await openSpray({ RTCPeerConnection });
      const hard = await openFleet({ RTCPeerConnection, cert, creds, offerSdp: easy.offerSdp, size: fleet });
      await until(() => hard.ports().length >= 1);
      // Spray only low decoy ports; the fleet binds high ports, so none is hit.
      await easy.applyAnswer(hard.answerSdp, '127.0.0.1', sprayPorts(60).filter((p) => p < 20000));
      const opened = await Promise.race([hard.done.then(() => true), sleep(4000).then(() => false)]);
      ok(!opened, 'with no real port sprayed, no connection opens');
      easy.pc.close(); hard.conns.forEach((c) => c.pc.close());
    }
    return res;
  }, fleet);
  for (const [pass, what] of out) { console.log(`${pass ? 'ok  ' : 'FAIL'} ${what}`); if (!pass) failed++; }
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
