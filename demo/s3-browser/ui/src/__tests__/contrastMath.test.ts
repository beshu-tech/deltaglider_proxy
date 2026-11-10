// Must-fail fixtures for the contrast guard itself: it must refuse what it
// claims to refuse (a token it cannot parse, a colour just below 4.5:1).
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { flatten, meetsAA, parseColor, ratio } from '../test/contrastMath';

test('a token that is not #rrggbb is refused, not passed', () => {
  for (const bad of ['hsl(0 0% 50%)', 'hsl(210, 10%, 40%)', 'red', 'transparent', 'var(--x)', '#12', '#12345', '', 'rgb(1,2)', 'rgb(a,b,c)']) {
    assert.throws(() => ratio(bad, '#ffffff'), /unparseable/, bad);
  }
});

test('shorthand hex and rgb() parse to the same colour as the long form', () => {
  assert.deepEqual(parseColor('#777'), parseColor('#777777'));
  assert.deepEqual(parseColor('rgb(119, 119, 119)'), parseColor('#777777'));
  assert.deepEqual(parseColor('rgba(0,0,0,0.5)'), [0, 0, 0, 0.5]);
  assert.equal(parseColor('#00000080')[3].toFixed(2), '0.50');
});

test('#777 on white is 4.48:1 and fails; the old slicing returned NaN and passed', () => {
  const r = ratio('#777', '#ffffff');
  assert.ok(Math.abs(r - 4.48) < 0.01, String(r));
  assert.equal(meetsAA(r), false);
});

test('NaN is never a pass', () => {
  assert.equal(meetsAA(NaN), false);
  assert.equal(meetsAA(4.499), false);
  assert.equal(meetsAA(4.5), true);
});

test('flatten composites alpha over the background', () => {
  assert.equal(flatten('rgba(0,0,0,0.5)', '#ffffff'), '#808080');
  assert.equal(flatten('#112233', '#ffffff'), '#112233');
});

test('a translucent colour is refused: ratio() cannot measure it, flatten() it first', () => {
  for (const c of ['rgba(255,255,255,0.1)', '#ffffff1a', '#fff1']) {
    assert.throws(() => ratio(c, '#000000'), /translucent/, c);
    assert.throws(() => ratio('#000000', c), /translucent/, c);
  }
  // Flattened, the same token measures as the low contrast it is.
  assert.ok(ratio(flatten('rgba(255,255,255,0.1)', '#000000'), '#000000') < 1.3);
  // An explicit alpha of 1 is opaque.
  assert.doesNotThrow(() => ratio('rgba(255,255,255,1)', '#000000'));
});
