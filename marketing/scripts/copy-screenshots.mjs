#!/usr/bin/env node
import { cp, mkdir, readdir, readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const src = join(root, '..', 'docs', 'screenshots');
const dst = join(root, 'public', 'screenshots');

const REQUIRED_SCREENSHOTS = [
  'filebrowser.jpg',
  'analytics.jpg',
  'iam.jpg',
  'advanced_security.jpg',
  'bucket-policies.jpg',
  'object-replication.jpg',
];

function jpegDimensions(buffer) {
  if (buffer[0] !== 0xff || buffer[1] !== 0xd8) {
    throw new Error('not a JPEG');
  }

  let offset = 2;
  while (offset < buffer.length) {
    if (buffer[offset] !== 0xff) {
      offset += 1;
      continue;
    }

    const marker = buffer[offset + 1];
    const length = buffer.readUInt16BE(offset + 2);
    if (
      marker === 0xc0 ||
      marker === 0xc1 ||
      marker === 0xc2 ||
      marker === 0xc3 ||
      marker === 0xc5 ||
      marker === 0xc6 ||
      marker === 0xc7 ||
      marker === 0xc9 ||
      marker === 0xca ||
      marker === 0xcb ||
      marker === 0xcd ||
      marker === 0xce ||
      marker === 0xcf
    ) {
      return {
        height: buffer.readUInt16BE(offset + 5),
        width: buffer.readUInt16BE(offset + 7),
      };
    }
    offset += 2 + length;
  }

  throw new Error('could not find JPEG dimensions');
}

async function assertRequiredScreenshots() {
  const failures = [];
  for (const name of REQUIRED_SCREENSHOTS) {
    const path = join(src, name);
    try {
      const bytes = await readFile(path);
      const { width, height } = jpegDimensions(bytes);
      if (width < 900 || height < 700) {
        failures.push(`${name}: ${width}×${height}, expected at least 900×700`);
      }
    } catch (err) {
      failures.push(`${name}: ${err.message}`);
    }
  }

  if (failures.length > 0) {
    console.error('copy-screenshots: required screenshot check failed');
    for (const failure of failures) {
      console.error(`✗ ${failure}`);
    }
    process.exit(1);
  }
}

await assertRequiredScreenshots();
await mkdir(dst, { recursive: true });

const entries = await readdir(src, { withFileTypes: true });
let copied = 0;
for (const entry of entries) {
  if (!entry.isFile()) continue;
  if (entry.name.startsWith('.')) continue;
  await cp(join(src, entry.name), join(dst, entry.name));
  copied += 1;
}

console.log(`copy-screenshots: ${copied} file(s) → public/screenshots/`);
