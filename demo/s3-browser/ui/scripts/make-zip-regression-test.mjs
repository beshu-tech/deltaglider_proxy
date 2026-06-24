// Regression test for the hand-rolled ZIP byte-writer (src/makeZip.ts).
// Guards the demo data generator against silently producing CORRUPT zips —
// a header offset / u32-packing / CRC mistake would break "download opens".
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { gunzipSync, inflateRawSync } from 'node:zlib';
import ts from 'typescript';

async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const url = await loadModule('../src/makeZip.ts', 'makeZip.ts');
const { crc32, makeZip, makeReleaseZip } = await import(url);

// — CRC-32: known IEEE test vectors —
const te = new TextEncoder();
assert.equal(crc32(te.encode('')) >>> 0, 0x00000000, 'crc32("") === 0');
assert.equal(crc32(te.encode('123456789')) >>> 0, 0xcbf43926, 'crc32 check value');

// — A stored-entry zip is structurally valid —
function parseZip(bytes) {
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  // EOCD is the last 22 bytes (no comment).
  const eocdOff = bytes.length - 22;
  assert.equal(dv.getUint32(eocdOff, true), 0x06054b50, 'EOCD signature');
  const total = dv.getUint16(eocdOff + 10, true);
  const cdSize = dv.getUint32(eocdOff + 12, true);
  const cdOff = dv.getUint32(eocdOff + 16, true);
  assert.equal(total, 1, 'exactly one entry');
  // Local header at 0.
  assert.equal(dv.getUint32(0, true), 0x04034b50, 'local file header signature');
  const method = dv.getUint16(8, true);
  assert.equal(method, 0, 'STORED (method 0), not DEFLATE');
  const crcStored = dv.getUint32(14, true) >>> 0;
  const compSize = dv.getUint32(18, true);
  const uncompSize = dv.getUint32(22, true);
  assert.equal(compSize, uncompSize, 'stored: compressed === uncompressed size');
  const nameLen = dv.getUint16(26, true);
  const extraLen = dv.getUint16(28, true);
  const dataStart = 30 + nameLen + extraLen;
  const data = bytes.slice(dataStart, dataStart + uncompSize);
  // Central directory signature at its declared offset.
  assert.equal(dv.getUint32(cdOff, true), 0x02014b50, 'central directory signature');
  assert.equal(cdOff + cdSize, eocdOff, 'CD offset + size lands exactly at EOCD');
  // CRC in the header must match the actual data.
  assert.equal(crc32(data) >>> 0, crcStored, 'stored CRC matches data');
  return { data, name: new TextDecoder().decode(bytes.slice(30, 30 + nameLen)) };
}

const z = makeZip('release/manifest.txt', 'hello world');
assert.equal(z[0], 0x50, 'starts with PK');
assert.equal(z[1], 0x4b);
const parsed = parseZip(z);
assert.equal(parsed.name, 'release/manifest.txt', 'entry name round-trips');
assert.equal(new TextDecoder().decode(parsed.data), 'hello world', 'content round-trips');

// — Empty content + multi-byte UTF-8 entry name don't corrupt the structure —
parseZip(makeZip('a/b.txt', ''));
parseZip(makeZip('ünïcödé/nâme.txt', 'çontent ✓'));

// — Versioned zips: valid, and near-identical (the delta-demo property) —
const versions = [1, 2, 3, 4, 5].map(makeReleaseZip);
versions.forEach((v, i) => {
  const p = parseZip(v);
  assert.ok(p.data.length > 0, `v${i + 1} has content`);
});
const lens = new Set(versions.map((v) => v.length));
assert.equal(lens.size, 1, 'all versions are the same length (only bytes differ)');
let diff = 0;
for (let i = 0; i < versions[0].length; i++) if (versions[0][i] !== versions[1][i]) diff++;
assert.ok(diff > 0 && diff < 50, `consecutive versions differ by a few bytes (got ${diff})`);

console.log('make-zip regression: OK');
