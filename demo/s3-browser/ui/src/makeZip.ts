// Pure ZIP byte-writer for the demo data generator. No DOM/React deps so it's
// unit-testable from Node (see scripts/make-zip-regression-test.mjs).

// CRC-32 (IEEE) — required by the ZIP format. Table built once.
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

/** @public — exercised by scripts/make-zip-regression-test.mjs (dynamic import). */
export function crc32(bytes: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) c = CRC_TABLE[(c ^ bytes[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// A REAL, openable ZIP, single STORED (uncompressed) entry — on purpose: a tiny
// content change stays a tiny binary delta (DEFLATE would scramble it).
/** @public — exercised by scripts/make-zip-regression-test.mjs (dynamic import). */
export function makeZip(entryName: string, content: string): Uint8Array {
  const enc = new TextEncoder();
  const name = enc.encode(entryName);
  const data = enc.encode(content);
  const crc = crc32(data);
  const u16 = (n: number) => [n & 0xff, (n >> 8) & 0xff];
  const u32 = (n: number) => [n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >>> 24) & 0xff];

  // Local file header + data
  const local = [
    ...u32(0x04034b50), ...u16(20), ...u16(0), ...u16(0), ...u16(0), ...u16(0),
    ...u32(crc), ...u32(data.length), ...u32(data.length),
    ...u16(name.length), ...u16(0), ...name, ...data,
  ];
  // Central directory header
  const central = [
    ...u32(0x02014b50), ...u16(20), ...u16(20), ...u16(0), ...u16(0), ...u16(0), ...u16(0),
    ...u32(crc), ...u32(data.length), ...u32(data.length),
    ...u16(name.length), ...u16(0), ...u16(0), ...u16(0), ...u16(0), ...u32(0), ...u32(0), ...name,
  ];
  // End of central directory
  const eocd = [
    ...u32(0x06054b50), ...u16(0), ...u16(0), ...u16(1), ...u16(1),
    ...u32(central.length), ...u32(local.length), ...u16(0),
  ];
  return new Uint8Array([...local, ...central, ...eocd]);
}

// Versioned "release" zip. Payload is mostly stable across versions with a few
// changing lines — so delta compression dedups the shared bulk.
export function makeReleaseZip(version: number): Uint8Array {
  const stable = Array.from({ length: 800 }, (_, i) => `config.entry.${i} = value-${i}`).join('\n');
  const changelog = `app v1.${version}.0\nbuilt: release ${version}\nfeature flag ${version} enabled\n`;
  return makeZip('release/manifest.txt', `${changelog}\n${stable}\n`);
}
