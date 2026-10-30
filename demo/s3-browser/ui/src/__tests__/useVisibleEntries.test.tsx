// useVisibleEntries: search filter and hidden DG system folders.
import { act } from '@testing-library/react';
import { afterEach, expect, test } from 'vitest';
import { renderHookWithQuery } from '../test/render';
import { useVisibleEntries } from '../browser/useVisibleEntries';
import { readStorage, removeStorage } from '../safeStorage';
import type { S3Object } from '../types';

const obj = (key: string) => ({ key, size: 1, lastModified: '' }) as S3Object;
const OBJECTS = [obj('builds/app-1.0.zip'), obj('builds/README.md')];
const FOLDERS = ['builds/.dg/', 'builds/.deltaglider/', 'builds/nightly/', 'builds/ci/'];
const VIRTUAL = ['builds/ci/'];

afterEach(() => removeStorage('dg-show-hidden'));

test('system folders are hidden by default', () => {
  const { result } = renderHookWithQuery(() => useVisibleEntries(OBJECTS, FOLDERS, VIRTUAL, ''));
  expect(result.current.folders).toEqual(['builds/nightly/', 'builds/ci/']);
  expect(result.current.objects).toBe(OBJECTS);
  expect(result.current.virtualFolders).toEqual(['builds/ci/']);
});

test('"Show system files" shows them and persists', () => {
  const { result } = renderHookWithQuery(() => useVisibleEntries(OBJECTS, FOLDERS, VIRTUAL, ''));
  act(() => result.current.setShowHidden(true));
  expect(result.current.folders).toEqual(FOLDERS);
  expect(readStorage('dg-show-hidden')).toBe('true');
  // A new mount reads the stored choice.
  const again = renderHookWithQuery(() => useVisibleEntries(OBJECTS, FOLDERS, VIRTUAL, ''));
  expect(again.result.current.showHidden).toBe(true);
});

test('search filters objects and folders, case-insensitive', () => {
  const { result } = renderHookWithQuery(() => useVisibleEntries(OBJECTS, FOLDERS, VIRTUAL, 'APP'));
  expect(result.current.objects.map((o) => o.key)).toEqual(['builds/app-1.0.zip']);
  expect(result.current.folders).toEqual([]);
  expect(result.current.virtualFolders).toEqual([]);
});

test('the folder list keeps its identity across renders (no /usage loop)', () => {
  const { result, rerender } = renderHookWithQuery(() => useVisibleEntries(OBJECTS, FOLDERS, VIRTUAL, ''));
  const first = result.current.folders;
  rerender();
  expect(result.current.folders).toBe(first);
});
