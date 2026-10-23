// NB-048: the deterministic primitives of the browser birthday punch (src/birthday.js):
// shared ICE credentials, the spray port order, guessed-port candidates, SDP munging.
// Pure functions: no browser, no network.
//
//   node packages/js/test/birthday.mjs
import { randomCredentials, shareCredentials, sprayPorts, sprayCandidates, isPublicV4, stripCandidates, FLEET_SIZE, SPRAY_PORTS } from '../src/birthday.js';

let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
// A deterministic [0,1) source from a seed, for repeatable ports/credentials.
function seeded(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

// Credentials: the right shape (RFC 8839 lengths, allowed alphabet) and random.
const creds = randomCredentials();
ok(/^pw[A-Za-z0-9+/]{6}$/.test(creds.ufrag) && creds.pwd.length === 24 && /^[A-Za-z0-9+/]+$/.test(creds.pwd), `credentials ${creds.ufrag} / ${creds.pwd.length} chars`);
ok(randomCredentials().ufrag !== randomCredentials().ufrag, 'credentials are random');

// shareCredentials replaces only the two lines, keeps the rest, is reversible shape.
const sdp = ['v=0', 'o=- 1 2 IN IP4 0.0.0.0', 's=-', 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
  'a=ice-ufrag:ORIG', 'a=ice-pwd:origpasswordoriginalpwd00', 'a=fingerprint:sha-256 AA:BB', 'a=setup:actpass'].join('\r\n');
const shared = shareCredentials(sdp, creds);
ok(shared.includes(`a=ice-ufrag:${creds.ufrag}`) && shared.includes(`a=ice-pwd:${creds.pwd}`), 'ufrag and pwd replaced');
ok(!shared.includes('ORIG') && !shared.includes('origpassword') && shared.includes('a=fingerprint:sha-256 AA:BB') && shared.includes('a=setup:actpass'), 'everything else kept');
// Two connections munged to the same credentials match exactly on those lines.
const a = shareCredentials(sdp.replace('ORIG', 'XXXX'), creds), b = shareCredentials(sdp.replace('ORIG', 'YYYY'), creds);
ok(/a=ice-ufrag:(\S+)/.exec(a)[1] === /a=ice-ufrag:(\S+)/.exec(b)[1] && /a=ice-pwd:(\S+)/.exec(a)[1] === /a=ice-pwd:(\S+)/.exec(b)[1], 'two connections share the munged credentials');
// Bad credentials (too short, bad chars, injection) leave the SDP unchanged.
for (const bad of [{ ufrag: 'ab', pwd: creds.pwd }, { ufrag: creds.ufrag, pwd: 'short' }, { ufrag: 'a\r\nevil', pwd: creds.pwd }, null]) {
  ok(shareCredentials(sdp, bad) === sdp, `bad credentials refused: ${JSON.stringify(bad)}`);
}

// Spray ports: `count` distinct ports in [1024, 65535], each once; deterministic per seed.
const ports = sprayPorts(SPRAY_PORTS, seeded(1));
ok(ports.length === SPRAY_PORTS, `${ports.length} ports`);
ok(new Set(ports).size === ports.length, 'all distinct');
ok(ports.every((p) => p >= 1024 && p <= 65535), `all in [1024, 65535] (${Math.min(...ports)}..${Math.max(...ports)})`);
ok(JSON.stringify(sprayPorts(SPRAY_PORTS, seeded(1))) === JSON.stringify(ports), 'deterministic per seed');
ok(JSON.stringify(sprayPorts(SPRAY_PORTS, seeded(2))) !== JSON.stringify(ports), 'another seed, another order');
// The whole range can be covered with no repeats (stride coprime to the range).
const full = sprayPorts(64512, seeded(7));
ok(new Set(full).size === 64512, 'the full range is a permutation (no repeats)');

// Guessed candidates: one host candidate per port at the hard IP, distinct foundations.
const cands = sprayCandidates('203.0.113.9', ports.slice(0, 5), 'data');
ok(cands.length === 5 && cands.every((c) => / 203\.0\.113\.9 \d+ typ host/.test(c.candidate) && c.sdpMid === 'data'), 'candidates at the hard IP, typ host, given mid');
ok(new Set(cands.map((c) => c.candidate.split(' ')[0])).size === 5, 'distinct foundations (each its own pair)');
ok(cands.map((c) => Number(c.candidate.split(' ')[5])).join() === ports.slice(0, 5).join(), 'ports in spray order');
// Untrusted IP: only a plain IPv4 address yields candidates, so nothing splices in.
for (const bad of ['203.0.113.9 9 typ host', '203.0.113.9\r\na=evil', '2001:db8::1', '256.0.0.1', '', null]) {
  ok(sprayCandidates(bad, [40000]).length === 0, `bad IP refused: ${JSON.stringify(bad)}`);
}

ok(FLEET_SIZE === 256 && SPRAY_PORTS === 1024, `fleet ${FLEET_SIZE}, spray ${SPRAY_PORTS}`);

// isPublicV4 (NB48-1/NB48-2): only a routable public IPv4 passes, so a punch can't be
// aimed at the victim's own LAN, router, loopback or carrier-NAT range.
for (const good of ['203.0.113.9', '8.8.8.8', '157.49.45.3', '1.1.1.1']) ok(isPublicV4(good), `public: ${good}`);
for (const bad of ['10.0.0.1', '192.168.1.5', '172.16.0.1', '172.31.255.1', '127.0.0.1', '0.0.0.0', '169.254.1.1', '100.64.0.1', '100.127.0.1', '224.0.0.1', '255.255.255.255', '2001:db8::1', 'nope', '', null]) {
  ok(!isPublicV4(bad), `not public: ${JSON.stringify(bad)}`);
}
// 172.15 and 172.32 are public (only 172.16–31 is private); 100.63 and 100.128 too.
ok(isPublicV4('172.15.0.1') && isPublicV4('172.32.0.1') && isPublicV4('100.63.0.1') && isPublicV4('100.128.0.1'), 'private-range edges');

// stripCandidates (NB48b-3): Chrome splits SDP on `\n` only, so after the strip no line
// that split yields may be a candidate, whatever the line endings, and every other line
// is kept as it was. (A regex line model kept the `\r\r\n`, stray `\r` and U+2028 cases.)
const cand = (k) => `a=candidate:inj${k} 1 udp 1 127.0.0.1 ${22000 + k} typ host`;
const endings = {
  'CRLF': (k) => `${cand(k)}\r\n`,
  'LF': (k) => `${cand(k)}\n`,
  'CR CR LF': (k) => `${cand(k)}\r\r\n`,
  'stray CR at the end': (k) => `${cand(k)} generation 0\r\r\n`,
  'stray CR inside': (k) => `${cand(k).replace(' typ', '\r typ')}\r\n`,
  'U+2028 inside': (k) => `${cand(k)}\u2028x\r\n`,
  'U+2028 then another candidate': (k) => `${cand(k)}\u2028${cand(k + 10)}\r\n`,
  'upper case, leading space': (k) => ` A=CANDIDATE:inj${k} 1 udp 1 127.0.0.1 ${22000 + k} typ host\r\n`,
};
const isCand = (l) => /^\s*a=candidate:/i.test(l);
for (const [name, line] of Object.entries(endings)) {
  for (const eol of ['\r\n', '\n']) {
    const lines = sdp.split('\r\n');
    const hostile = lines.slice(0, 4).join(eol) + eol + line(1) + lines.slice(4).join(eol) + eol + line(2);
    const out = stripCandidates(hostile);
    const left = out.split('\n').filter(isCand).length;
    ok(left === 0 && out === lines.join(eol) + eol, `strip, ${name} (SDP lines end ${JSON.stringify(eol)}): ${left} candidate lines left, the rest unchanged`);
  }
}

// MIC-1: gathering writes the default candidate (a host one when no STUN server answered)
// into the connection line and the m-line port; the strip puts both back as they were
// before gathering, so the SDP names no address at all.
for (const c of ['c=IN IP4 10.0.0.170', 'c=IN IP6 2001:db8::5']) {
  const gathered = ['v=0', 'm=application 56167 UDP/DTLS/SCTP webrtc-datachannel', c, 'a=rtcp:56167 IN IP4 10.0.0.170',
    'a=candidate:1 1 udp 2122260223 10.0.0.170 56167 typ host', 'a=candidate:2 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 10.0.0.170 rport 56167', 'a=mid:0', ''].join('\r\n');
  const out = stripCandidates(gathered);
  ok(out === ['v=0', 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0', 'a=rtcp:9 IN IP4 0.0.0.0', 'a=mid:0', ''].join('\r\n'),
    `strip, a gathered SDP (${c}): no candidate, c= 0.0.0.0, port 9${out.includes('10.0.0.170') || out.includes('2001:db8') ? `; left: ${JSON.stringify(out)}` : ''}`);
}

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
