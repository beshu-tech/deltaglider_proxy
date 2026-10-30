// useSelection: checked rows of the object browser; folders are `folder:<prefix>`.
import { act } from '@testing-library/react';
import { expect, test } from 'vitest';
import { renderHookWithQuery } from '../test/render';
import useSelection from '../useSelection';
import type { S3Object } from '../types';

const obj = (key: string) => ({ key, size: 1, lastModified: '' }) as S3Object;
const OBJECTS = [obj('a.zip'), obj('b.zip')];
const FOLDERS = ['nightly/'];

test('toggleKey checks and unchecks one row', () => {
  const { result } = renderHookWithQuery(() => useSelection());
  act(() => result.current.toggleKey('a.zip'));
  expect([...result.current.selectedKeys]).toEqual(['a.zip']);
  act(() => result.current.toggleKey('a.zip'));
  expect(result.current.selectedKeys.size).toBe(0);
});

test('toggleAll checks every given row, then none', () => {
  const { result } = renderHookWithQuery(() => useSelection());
  act(() => result.current.toggleAll(OBJECTS, FOLDERS));
  expect([...result.current.selectedKeys].sort()).toEqual(['a.zip', 'b.zip', 'folder:nightly/']);
  act(() => result.current.toggleAll(OBJECTS, FOLDERS));
  expect(result.current.selectedKeys.size).toBe(0);
});

test('toggleAll after a partial selection checks every row', () => {
  const { result } = renderHookWithQuery(() => useSelection());
  act(() => result.current.toggleKey('a.zip'));
  act(() => result.current.toggleAll(OBJECTS, FOLDERS));
  expect(result.current.selectedKeys.size).toBe(3);
});

test('reconcile drops rows that are gone and keeps the set when nothing changed', () => {
  const { result } = renderHookWithQuery(() => useSelection());
  act(() => result.current.toggleAll(OBJECTS, FOLDERS));
  const before = result.current.selectedKeys;
  act(() => result.current.reconcile(OBJECTS, FOLDERS));
  expect(result.current.selectedKeys).toBe(before);
  act(() => result.current.reconcile([obj('b.zip')], []));
  expect([...result.current.selectedKeys]).toEqual(['b.zip']);
});

test('clearSelection empties it', () => {
  const { result } = renderHookWithQuery(() => useSelection());
  act(() => result.current.toggleKey('a.zip'));
  act(() => result.current.clearSelection());
  expect(result.current.selectedKeys.size).toBe(0);
});
