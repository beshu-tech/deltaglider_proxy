import assert from 'node:assert/strict';
import { test } from 'vitest';
import { normalizeDestPrefix, destinationIsSource, keyPathError } from '../components/destPrefix';

test('normalizeDestPrefix: truth table (from the bug report)', () => {
  assert.equal(normalizeDestPrefix('foo//bar'), 'foo/bar'); // THE regression: internal // collapsed
  assert.equal(normalizeDestPrefix('/foo/'), 'foo'); // leading + trailing stripped
  assert.equal(normalizeDestPrefix('a///b//c'), 'a/b/c'); // multiple internal runs collapsed
  assert.equal(normalizeDestPrefix(''), ''); // empty -> bucket root
  assert.equal(normalizeDestPrefix('///'), ''); // all slashes -> bucket root
  assert.equal(normalizeDestPrefix('foo/bar'), 'foo/bar'); // valid input unchanged (no S3 shape change)
});

test('normalizeDestPrefix: additional well-formed inputs pass through untouched', () => {
  assert.equal(normalizeDestPrefix('foo'), 'foo');
  assert.equal(normalizeDestPrefix('foo/bar/baz'), 'foo/bar/baz');
  assert.equal(normalizeDestPrefix('//foo//bar//'), 'foo/bar'); // leading+internal+trailing combined
});

test('normalizeDestPrefix: idempotence — normalizing twice equals normalizing once', () => {
  for (const s of ['foo//bar', '/a///b/', '', '///', 'x/y/z', 'a//b//c//']) {
    assert.equal(normalizeDestPrefix(normalizeDestPrefix(s)), normalizeDestPrefix(s), `idempotent for ${JSON.stringify(s)}`);
  }
});

test('normalizeDestPrefix: fuzz — any slash arrangement is well-formed', () => {
  // Build random strings from {slash, segment chars} and assert the post-conditions:
  // never '//', never a leading/trailing slash.
  function randomSlashy(rng: () => number): string {
    const alphabet = ['/', '/', '/', 'a', 'b', 'c', '-', '.', '_'];
    const len = Math.floor(rng() * 24);
    let out = '';
    for (let i = 0; i < len; i++) out += alphabet[Math.floor(rng() * alphabet.length)];
    return out;
  }

  // Tiny deterministic PRNG (mulberry32) so failures are reproducible.
  function mulberry32(seed: number): () => number {
    let a = seed >>> 0;
    return () => {
      a |= 0;
      a = (a + 0x6d2b79f5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  const rng = mulberry32(0x1234abcd);
  for (let i = 0; i < 5000; i++) {
    const input = randomSlashy(rng);
    const out = normalizeDestPrefix(input);
    assert.ok(!out.includes('//'), `no double slash for ${JSON.stringify(input)} -> ${JSON.stringify(out)}`);
    assert.ok(!out.startsWith('/'), `no leading slash for ${JSON.stringify(input)} -> ${JSON.stringify(out)}`);
    assert.ok(!out.endsWith('/'), `no trailing slash for ${JSON.stringify(input)} -> ${JSON.stringify(out)}`);
  }
});

test('destinationIsSource: the default destination (current folder)', () => {
  // Files in the browsed folder, destination = that folder -> self-copy.
  assert.equal(destinationIsSource('b', ['fw/a.bin', 'fw/b.bin'], 'b', 'fw'), true);
  assert.equal(destinationIsSource('b', ['fw/a.bin'], 'b', '/fw/'), true); // raw input normalized
  // A selected folder keeps its name: fw/v1/ copied into fw/ lands on fw/v1/.
  assert.equal(destinationIsSource('b', ['folder:fw/v1/'], 'b', 'fw'), true);
  assert.equal(destinationIsSource('b', ['folder:fw/v1/', 'fw/x'], 'b', 'fw/'), true);
  // Bucket root.
  assert.equal(destinationIsSource('b', ['a.txt', 'folder:dir/'], 'b', ''), true);
  // Different bucket or prefix -> real copy.
  assert.equal(destinationIsSource('b', ['fw/a.bin'], 'c', 'fw'), false);
  assert.equal(destinationIsSource('b', ['fw/a.bin'], 'b', 'fw/sub'), false);
  assert.equal(destinationIsSource('b', ['folder:fw/v1/'], 'b', 'fw/v1'), false); // into itself: new keys fw/v1/v1/...
  assert.equal(destinationIsSource('b', ['fw/a.bin'], 'b', ''), false);
  // Mixed parents: only some items would self-map -> not blocked.
  assert.equal(destinationIsSource('b', ['fw/a.bin', 'other/b.bin'], 'b', 'fw'), false);
  // Empty selection -> nothing to block.
  assert.equal(destinationIsSource('b', [], 'b', ''), false);
});

test('keyPathError: "." / ".." segments never reach a request', () => {
  // The browser's URL parser resolves `..` in a request path, so an upload to
  // `../x/` was signed for one path and sent to another (403
  // SignatureDoesNotMatch), and the proxy refuses such keys anyway
  // (`check_object_path` in src/api/admin/path_guard.rs).
  for (const bad of ['..', '.', '../x', 'a/../b', 'a/./b', 'a/..', '../', 'x/../', 'a\0b']) {
    assert.equal(typeof keyPathError(bad), 'string', `refused: ${JSON.stringify(bad)}`);
  }
  for (const ok of ['', 'a', 'a/b/', 'releases/v1.2.3/', '..a/', 'a../b', '.hidden/', 'a/.../b', 'a//b']) {
    assert.equal(keyPathError(ok), null, `allowed: ${JSON.stringify(ok)}`);
  }
});
