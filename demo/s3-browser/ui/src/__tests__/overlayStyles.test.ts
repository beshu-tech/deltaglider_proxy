/**
 * Regression test for the shared overlay-style builder.
 *
 * Covers the only non-trivial logic in `getOverlayBaseStyles`:
 *   - minWidth floor (`max(pos.width, minWidth)`) in both directions,
 *   - maxHeight pass-through,
 *   - flexLayout opt-in spreads display:flex / column (and omits it
 *     by default so plain dropdowns stay block),
 *   - the shared shadow / z-index / radius constants are wired in.
 */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { getOverlayBaseStyles, OVERLAY_SHADOW, Z_INDEX_OVERLAY, BORDER_RADIUS } from '../components/overlayStyles';
import type { useColors } from '../ThemeContext';

// Only the two fields getOverlayBaseStyles reads; the full ColorTokens shape
// is not needed to exercise the pure style-building logic under test.
const colors = { BG_ELEVATED: '#111', BORDER: '#333' } as unknown as ReturnType<typeof useColors>;

test('constants', () => {
  assert.deepEqual(OVERLAY_SHADOW, '0 8px 24px rgba(0,0,0,0.3)');
  assert.deepEqual(Z_INDEX_OVERLAY, 99999);
  assert.deepEqual(BORDER_RADIUS, { xs: 4, sm: 6, md: 8 });
});

test('minWidth floor', () => {
  const narrow = getOverlayBaseStyles(colors, { top: 10, left: 20, width: 50 }, {
    minWidth: 200,
    maxHeight: 240,
  });
  assert.deepEqual(narrow.width, 200, 'pos.width below floor → minWidth wins');
  assert.deepEqual(narrow.top, 10, 'top echoed from pos');
  assert.deepEqual(narrow.left, 20, 'left echoed from pos');
  assert.deepEqual(narrow.maxHeight, 240, 'maxHeight passed through');
  assert.deepEqual(narrow.position, 'fixed');
  assert.deepEqual(narrow.overflowY, 'auto');
  assert.deepEqual(narrow.background, '#111', 'background from colors');
  assert.deepEqual(narrow.border, '1px solid #333', 'border from colors');
  assert.deepEqual(narrow.borderRadius, BORDER_RADIUS.md);
  assert.deepEqual(narrow.boxShadow, OVERLAY_SHADOW);
  assert.deepEqual(narrow.zIndex, Z_INDEX_OVERLAY);

  const wide = getOverlayBaseStyles(colors, { top: 0, left: 0, width: 999 }, {
    minWidth: 220,
    maxHeight: 280,
  });
  assert.deepEqual(wide.width, 999, 'pos.width above floor wins');
});

test('flexLayout opt-in', () => {
  const narrow = getOverlayBaseStyles(colors, { top: 10, left: 20, width: 50 }, {
    minWidth: 200,
    maxHeight: 240,
  });
  assert.deepEqual(narrow.display, undefined, 'no flex layout by default');
  assert.deepEqual(narrow.flexDirection, undefined, 'no flex direction by default');

  const flex = getOverlayBaseStyles(colors, { top: 0, left: 0, width: 300 }, {
    minWidth: 220,
    maxHeight: 280,
    flexLayout: true,
  });
  assert.deepEqual(flex.display, 'flex', 'flexLayout spreads display:flex');
  assert.deepEqual(flex.flexDirection, 'column', 'flexLayout spreads column direction');
});
