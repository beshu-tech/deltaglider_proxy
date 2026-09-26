/**
 * Explore finding 7: the upload queue rendered one DOM row per file and
 * re-rendered all of them on every progress event. The list now renders
 * only the rows in view.
 */
import { render, screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import UploadProgressList from '../components/UploadProgressList';
import type { UploadQueueItem } from '../useUploadQueue';

function item(i: number): UploadQueueItem {
  return {
    id: `id-${i}`,
    file: new File(['x'], `f${i}.txt`),
    bucket: 'releases',
    destination: '',
    key: `f${i}.txt`,
    status: 'queued',
    originalSize: 1,
    transferredBytes: 0,
    totalBytes: 1,
    percent: 0,
    speedBytesPerSec: 0,
    totalParts: 0,
    completedParts: 0,
    inFlightParts: 0,
    activeConnections: 0,
    currentPart: null,
    startedAtMs: null,
    completingSinceMs: null,
    updatedAtMs: null,
    durationMs: null,
  };
}

// jsdom has no layout: give the scroll box and each row a size.
beforeEach(() => {
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(520);
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(800);
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
    x: 0, y: 0, top: 0, left: 0, bottom: 84, right: 800, width: 800, height: 84, toJSON: () => ({}),
  } as DOMRect);
});
afterEach(() => vi.restoreAllMocks());

const noop = () => {};
const colors = {
  borderColor: '#333', textPrimary: '#fff', textMuted: '#999', accentBlue: '#00f',
  accentGreen: '#0f0', accentRed: '#f00', finalizingColor: '#fa0',
};

test('a queue of 1000 files puts only the visible rows in the DOM', () => {
  const queue = Array.from({ length: 1000 }, (_, i) => item(i));
  render(<UploadProgressList queue={queue} onCancelUpload={noop} onRetryUpload={noop} {...colors} />);
  const rows = screen.getAllByRole('listitem');
  expect(rows.length).toBeGreaterThan(0);
  expect(rows.length).toBeLessThan(40);
  expect(rows[0]).toHaveAttribute('aria-setsize', '1000');
  expect(screen.getByRole('listitem', { name: /f0\.txt/ })).toBeInTheDocument();
  expect(screen.queryByRole('listitem', { name: /f999\.txt/ })).toBeNull();
});
