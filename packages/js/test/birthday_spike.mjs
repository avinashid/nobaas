// NB-048 feasibility spike: can a browser do the birthday punch that native nodes
// do (NB-044)? The hard side (a random-port symmetric NAT) must open many sockets
// toward the easy side's one address, and the easy side (a port-filtering cone)
// probes many ports until one lands. In a browser the only socket is inside an
// RTCPeerConnection, so the hard side needs MANY connections that look like ONE
// endpoint to the easy side — i.e. they must share one DTLS certificate and one set
// of ICE credentials, or the easy side cannot answer them all with one answer.
//
// This spike does NOT reproduce the NATs (loopback can't; the native simulator
// already proves the NAT maths). It answers only the browser-mechanism questions
// that decide whether the feature is buildable, and prints findings. It passes if
// the critical mechanism (shared certificate + munged ICE credentials that stick)
// works; it reports the rest.
//
//   node packages/js/test/birthday_spike.mjs
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let criticalOk = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  page.on('pageerror', (e) => console.log(`page error: ${e.message}`));
  await page.goto(`http://127.0.0.1:${port}/`);
  const out = await page.evaluate(async () => {
    const findings = [];
    const note = (k, v, good) => findings.push({ k, v, good });
    const ufragOf = (sdp) => /a=ice-ufrag:(\S+)/.exec(sdp)?.[1];
    const pwdOf = (sdp) => /a=ice-pwd:(\S+)/.exec(sdp)?.[1];
    const fpOf = (sdp) => /a=fingerprint:\S+ (\S+)/.exec(sdp)?.[1];

    // 1. One certificate shared across many connections → one DTLS fingerprint.
    let cert, sharedFp = false, manyMade = 0, makeMs = 0;
    try {
      cert = await RTCPeerConnection.generateCertificate({ name: 'ECDSA', namedCurve: 'P-256' });
      const a = new RTCPeerConnection({ certificates: [cert] });
      const b = new RTCPeerConnection({ certificates: [cert] });
      a.createDataChannel('x'); b.createDataChannel('x');
      await a.setLocalDescription(await a.createOffer());
      await b.setLocalDescription(await b.createOffer());
      sharedFp = fpOf(a.localDescription.sdp) && fpOf(a.localDescription.sdp) === fpOf(b.localDescription.sdp);
      a.close(); b.close();
    } catch (e) {
      note('generateCertificate', `threw: ${e.message}`, false);
    }
    note('shared certificate → same DTLS fingerprint', sharedFp ? 'yes' : 'no', sharedFp);

    // 2. THE CRITICAL ONE: munge the local ICE ufrag/pwd to fixed values and see if
    //    Chrome keeps them (so all the hard-side connections can share credentials).
    let mungeSticks = false, mungeErr = '';
    try {
      const pc = new RTCPeerConnection(cert ? { certificates: [cert] } : {});
      pc.createDataChannel('x');
      const offer = await pc.createOffer();
      const want = { u: 'pwbday0000000000', p: 'pwbdaypwdpwbdaypwdpwbdaypwd00' };
      const munged = offer.sdp.replace(/a=ice-ufrag:\S+/, `a=ice-ufrag:${want.u}`).replace(/a=ice-pwd:\S+/, `a=ice-pwd:${want.p}`);
      await pc.setLocalDescription({ type: 'offer', sdp: munged });
      mungeSticks = ufragOf(pc.localDescription.sdp) === want.u && pwdOf(pc.localDescription.sdp) === want.p;
      pc.close();
    } catch (e) {
      mungeErr = e.message;
    }
    note('munged local ICE ufrag/pwd stick (shared credentials possible)', mungeSticks ? 'yes' : mungeErr ? `no: ${mungeErr}` : 'no (Chrome overrode them)', mungeSticks);

    // 3. Make many connections quickly — resource feasibility on the hard side.
    {
      const t0 = performance.now();
      const pcs = [];
      try {
        for (let i = 0; i < 256; i++) { const pc = new RTCPeerConnection(cert ? { certificates: [cert] } : {}); pc.createDataChannel('x'); pcs.push(pc); }
        manyMade = pcs.length;
      } catch (e) {
        note('256 connections at once', `threw at ${pcs.length}: ${e.message}`, false);
      }
      makeMs = Math.round(performance.now() - t0);
      for (const pc of pcs) pc.close();
    }
    if (manyMade) note('256 connections at once', `made ${manyMade} in ${makeMs} ms`, manyMade === 256);

    // 4. How fast does Chrome send connectivity checks to many added remote
    //    candidates? (The easy side's spray rate.) Add many host candidates at
    //    loopback ports (only unused ones, so none completes) and count checks/s.
    let checksPerSec = 0, added = 0;
    try {
      const pc = new RTCPeerConnection(cert ? { certificates: [cert] } : {});
      pc.createDataChannel('x');
      await pc.setLocalDescription(await pc.createOffer());
      // A fake answer with one media section and credentials, no candidates.
      const ans = pc.localDescription.sdp
        .replace('a=setup:actpass', 'a=setup:active')
        .replace(/a=ice-ufrag:\S+/, 'a=ice-ufrag:answerufrag00')
        .replace(/a=ice-pwd:\S+/, 'a=ice-pwd:answerpwdanswerpwdanswerpwd00');
      try { await pc.setRemoteDescription({ type: 'answer', sdp: ans }); } catch (e) { note('synthetic answer accepted', `no: ${e.message}`, false); }
      for (let i = 0; i < 400; i++) {
        try { await pc.addIceCandidate({ candidate: `candidate:${i} 1 udp ${2000000 - i} 127.0.0.1 ${20000 + i} typ host`, sdpMid: '0', sdpMLineIndex: 0 }); added++; } catch { /* some rejected */ }
      }
      await new Promise((r) => setTimeout(r, 1500));
      let sent = 0;
      (await pc.getStats()).forEach((s) => { if (s.type === 'candidate-pair') sent += s.requestsSent ?? 0; });
      checksPerSec = Math.round(sent / 1.5);
      pc.close();
    } catch (e) {
      note('check pacing', `error: ${e.message}`, false);
    }
    note('remote candidates addIceCandidate accepts', `${added} of 400`, added > 0);
    if (checksPerSec) note('connectivity checks per second (spray rate)', `${checksPerSec}/s`, checksPerSec > 0);

    return findings;
  });

  console.log('\nbrowser birthday-punch feasibility\n');
  for (const f of out) console.log(`  ${f.good ? 'ok  ' : '··  '} ${f.k}: ${f.v}`);
  const critical = out.find((f) => f.k.startsWith('munged local ICE'));
  const shared = out.find((f) => f.k.startsWith('shared certificate'));
  criticalOk = !!(critical?.good && shared?.good);
  console.log(`\n  verdict: shared credentials across many connections are ${criticalOk ? 'POSSIBLE — the browser birthday punch is buildable' : 'NOT possible this way — need the per-connection-pair design or another approach'}\n`);
} catch (e) {
  console.log(`spike error: ${e.stack ?? e}`);
} finally {
  await browser?.close();
  server.kill();
}
process.exit(criticalOk ? 0 : 1);
