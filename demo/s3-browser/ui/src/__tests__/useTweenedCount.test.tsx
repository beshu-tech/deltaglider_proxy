/**
 * The test browser asks for reduced motion (src/test/setup.ts), so a
 * count-up shows its final value at once. A test then reads the number
 * that the server sent, with no wait on animation frames: a loaded host
 * stretched the 450 ms tween past the wait of a findByText.
 */
import { expect, test } from 'vitest';
import { useTweenedCount } from '../hooks/useTweenedCount';
import { renderHookWithQuery } from '../test/render';

test('a count-up shows its target at once in tests', () => {
  const { result, rerender } = renderHookWithQuery(() => useTweenedCount(1200));
  expect(result.current).toBe(1200);
  rerender();
  expect(result.current).toBe(1200);
});
