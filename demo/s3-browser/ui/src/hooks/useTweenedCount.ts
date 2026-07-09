import { useEffect, useRef, useState } from 'react';

/**
 * Smoothly animate a numeric value toward `target` using requestAnimationFrame
 * with a cubic ease-out curve (same algorithm as DeltaSavingsChip). Extracted
 * into a reusable hook so the verify-progress bar can share the same count-up.
 *
 * Two hard requirements (golden roadmap Tier 0.1.3):
 *  1. Honors `prefers-reduced-motion` — snaps instantly instead of easing.
 *  2. Snaps (don't ease) when the target drops below the displayed value —
 *     a reverify resets the count to 0, and easing DOWN looks like a bug.
 *
 * @param target   The value to animate toward.
 * @param duration Animation length in ms (default 450, same as the chip).
 * @returns        The currently-displayed (animated) value.
 */
export function useTweenedCount(target: number, duration = 450): number {
  const [displayed, setDisplayed] = useState(target);
  const displayedRef = useRef(displayed);

  // Keep ref in sync so the effect closure sees the latest value without
  // adding it to the dependency array (avoids re-triggering the tween).
  displayedRef.current = displayed;

  useEffect(() => {
    const startVal = displayedRef.current;
    const delta = target - startVal;

    // Snap when: target hasn't moved, or target decreased (reverify reset).
    // Easing downward looks like the bar is going backwards — a visual bug.
    if (delta <= 0) {
      setDisplayed(target);
      return;
    }

    // Honor reduced-motion: snap instead of animate.
    const prefersReducedMotion =
      typeof window !== 'undefined' &&
      window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
    if (prefersReducedMotion) {
      setDisplayed(target);
      return;
    }

    let raf = 0;
    const startedAt = performance.now();
    const tick = (now: number) => {
      const t = Math.min(1, (now - startedAt) / duration);
      const eased = 1 - Math.pow(1 - t, 3);
      setDisplayed(startVal + delta * eased);
      if (t < 1) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target, duration]);

  return displayed;
}
