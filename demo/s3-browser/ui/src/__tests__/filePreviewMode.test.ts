// Regression test for getPreviewMode — the pure resolver that decides how the
// object browser previews a file (text / image / video / audio / none). Adding
// video + audio must not regress text/image detection, and extensions must be
// matched case-insensitively off the final path segment.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { getPreviewMode } from '../components/filePreviewMode';

test('getPreviewMode: video', () => {
  for (const f of ['demo.mp4', 'clip.webm', 'movie.MOV', 'a/b/render.m4v', 'screen.ogv']) {
    assert.equal(getPreviewMode(f), 'video', `expected video for ${f}`);
  }
});

test('getPreviewMode: audio', () => {
  for (const f of ['song.mp3', 'voice.WAV', 'track.flac', 'note.m4a', 'bed.ogg', 'pod.opus']) {
    assert.equal(getPreviewMode(f), 'audio', `expected audio for ${f}`);
  }
});

test('getPreviewMode: image (unchanged)', () => {
  for (const f of ['logo.png', 'shot.JPEG', 'icon.svg', 'pic.webp']) {
    assert.equal(getPreviewMode(f), 'image', `expected image for ${f}`);
  }
});

test('getPreviewMode: text (unchanged)', () => {
  for (const f of ['readme.md', 'config.yaml', 'data.json', 'Dockerfile', 'CHANGELOG']) {
    assert.equal(getPreviewMode(f), 'text', `expected text for ${f}`);
  }
});

test('getPreviewMode: none', () => {
  for (const f of ['archive.tar', 'blob.bin', 'firmware.zip', 'noext', 'a.b.unknownext']) {
    assert.equal(getPreviewMode(f), null, `expected null for ${f}`);
  }
});

test('getPreviewMode: extension comes from the LAST dot of the LAST path segment', () => {
  assert.equal(getPreviewMode('releases/v1.2.3/demo.mp4'), 'video');
  assert.equal(getPreviewMode('weird.mp4/notavideo.txt'), 'text');
});
