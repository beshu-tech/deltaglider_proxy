/** useDirtySection: per-panel dirty state, the shared refcount, ⌘S, tab title. */
import { act, renderHook } from '@testing-library/react';
import { describe, expect, test, vi } from 'vitest';
import {
  getDirtySections,
  requestApplyFirst,
  subscribeToDirtyState,
  useApplyHandler,
  useDirtyGlobalIndicators,
  useDirtySection,
} from '../useDirtySection';

describe('useDirtySection', () => {
  test('dirty tracks structural difference from the snapshot, key order ignored', () => {
    const { result } = renderHook(() => useDirtySection('t/order', { a: 1, b: { c: 2, d: 3 } }));
    expect(result.current.isDirty).toBe(false);
    // Same value, different key order (what serde may send back): not dirty.
    act(() => result.current.setValue({ b: { d: 3, c: 2 }, a: 1 }));
    expect(result.current.isDirty).toBe(false);
    act(() => result.current.setValue((v) => ({ ...v, a: 2 })));
    expect(result.current.isDirty).toBe(true);
  });

  test('array order is significant', () => {
    const { result } = renderHook(() => useDirtySection('t/arr', ['x', 'y']));
    act(() => result.current.setValue(['y', 'x']));
    expect(result.current.isDirty).toBe(true);
  });

  test('markApplied makes the current value the snapshot; resetWith replaces both', () => {
    const { result } = renderHook(() => useDirtySection('t/apply', { n: 1 }));
    act(() => result.current.setValue({ n: 2 }));
    act(() => result.current.markApplied());
    expect(result.current.isDirty).toBe(false);
    expect(result.current.value).toEqual({ n: 2 });
    act(() => result.current.setValue({ n: 3 }));
    act(() => result.current.discard());
    expect(result.current.value).toEqual({ n: 2 });
    act(() => result.current.resetWith({ n: 9 }));
    expect(result.current.value).toEqual({ n: 9 });
    expect(result.current.isDirty).toBe(false);
  });

  test('callbacks keep their identity across renders', () => {
    const { result, rerender } = renderHook(() => useDirtySection('t/stable', 0));
    const first = result.current;
    act(() => result.current.setValue(5));
    rerender();
    expect(result.current.setValue).toBe(first.setValue);
    expect(result.current.discard).toBe(first.discard);
    expect(result.current.markApplied).toBe(first.markApplied);
    expect(result.current.resetWith).toBe(first.resetWith);
  });

  test('two panels on one key: one going clean does not clear the other (refcount)', () => {
    const a = renderHook(() => useDirtySection('t/shared', 0));
    const b = renderHook(() => useDirtySection('t/shared', 0));
    const listener = vi.fn();
    const unsub = subscribeToDirtyState(listener);
    act(() => a.result.current.setValue(1));
    act(() => b.result.current.setValue(1));
    expect(getDirtySections().has('t/shared')).toBe(true);
    act(() => a.result.current.discard());
    expect(getDirtySections().has('t/shared')).toBe(true);
    b.unmount();
    expect(getDirtySections().has('t/shared')).toBe(false);
    expect(listener).toHaveBeenCalled();
    unsub();
    a.unmount();
  });

  test('unmounting a dirty panel releases its key', () => {
    const { result, unmount } = renderHook(() => useDirtySection('t/unmount', 'a'));
    act(() => result.current.setValue('b'));
    expect(getDirtySections().has('t/unmount')).toBe(true);
    unmount();
    expect(getDirtySections().has('t/unmount')).toBe(false);
  });
});

describe('useApplyHandler / requestApplyFirst', () => {
  test('dispatches to the most recently mounted enabled handler, first key wins', () => {
    const older = vi.fn();
    const newer = vi.fn();
    const other = vi.fn();
    const h1 = renderHook(() => useApplyHandler('t/k1', older, true));
    const h2 = renderHook(() => useApplyHandler('t/k1', newer, true));
    const h3 = renderHook(() => useApplyHandler('t/k2', other, true));
    expect(requestApplyFirst(['t/none', 't/k1', 't/k2'])).toBe(true);
    expect(newer).toHaveBeenCalledTimes(1);
    expect(older).not.toHaveBeenCalled();
    expect(other).not.toHaveBeenCalled();
    h2.unmount();
    requestApplyFirst(['t/k1']);
    expect(older).toHaveBeenCalledTimes(1);
    h1.unmount();
    h3.unmount();
    expect(requestApplyFirst(['t/k1', 't/k2'])).toBe(false);
  });

  test('a disabled handler is not reachable; the latest closure runs', () => {
    let enabled = false;
    let label = 'first';
    const seen: string[] = [];
    const { rerender, unmount } = renderHook(() => useApplyHandler('t/closure', () => seen.push(label), enabled));
    expect(requestApplyFirst(['t/closure'])).toBe(false);
    enabled = true;
    rerender();
    label = 'second';
    rerender();
    requestApplyFirst(['t/closure']);
    expect(seen).toEqual(['second']);
    unmount();
  });
});

describe('useDirtyGlobalIndicators', () => {
  test('prefixes the tab title and guards unload while anything is dirty', () => {
    document.title = 'Users · Settings';
    const indicators = renderHook(() => useDirtyGlobalIndicators());
    const panel = renderHook(() => useDirtySection('t/title', 0));
    expect(document.title).toBe('Users · Settings');

    act(() => panel.result.current.setValue(1));
    expect(document.title).toBe('● Users · Settings');
    const ev = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);

    act(() => panel.result.current.discard());
    expect(document.title).toBe('Users · Settings');
    const clean = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(clean);
    expect(clean.defaultPrevented).toBe(false);

    panel.unmount();
    indicators.unmount();
  });
});
