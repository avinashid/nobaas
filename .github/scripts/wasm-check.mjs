// Checks the browser wasm module: size budget and zero imports.
//
//   node .github/scripts/wasm-check.mjs [path=packages/js/pkg/peerwire_ffi.wasm] [max-bytes=$WASM_MAX_BYTES or 358400]

import { readFileSync } from 'node:fs';

const file = process.argv[2] ?? 'packages/js/pkg/peerwire_ffi.wasm';
const max = Number(process.argv[3] ?? process.env.WASM_MAX_BYTES ?? 358400); // same default as scripts/gate.sh
const bytes = readFileSync(file);
const mod = new WebAssembly.Module(bytes);
const imports = WebAssembly.Module.imports(mod);
const exports = WebAssembly.Module.exports(mod);
console.log(`${file}: ${bytes.length} bytes (${(bytes.length / 1024).toFixed(1)} KiB, budget ${max / 1024} KiB), ${imports.length} imports, ${exports.length} exports`);
for (const i of imports) console.log(`  import ${i.module}.${i.name} (${i.kind})`);
let ok = true;
if (bytes.length > max) { console.log('FAIL: wasm is over the size budget'); ok = false; }
if (imports.length > 0) { console.log('FAIL: wasm must have zero imports'); ok = false; }
process.exit(ok ? 0 : 1);
