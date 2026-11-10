import { expect, test } from 'vitest';
import { memberDelta } from '../adminApi';

test('memberDelta is the change against the starting list, and nothing when unchanged', () => {
  expect(memberDelta([1, 2, 3], new Set([2, 3, 4]))).toEqual({ add_member_ids: [4], remove_member_ids: [1] });
  expect(memberDelta([], new Set([5, 3]))).toEqual({ add_member_ids: [3, 5] });
  expect(memberDelta([1, 2], new Set())).toEqual({ remove_member_ids: [1, 2] });
  expect(memberDelta([1, 2], new Set([2, 1]))).toEqual({});
});
