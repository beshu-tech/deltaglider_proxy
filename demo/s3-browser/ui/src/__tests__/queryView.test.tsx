/**
 * UX harvest D16/D17: a page showed "No audit entries yet" under a failed
 * request, and the empty "Create the first user" state while the list was
 * still loading. One decision now picks exactly one state.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, test, vi } from 'vitest';
import { ApiError } from '../errorHandling';
import { queriesView, queryView } from '../queryView';
import { EmptyState, QueryState } from '../components/StatePlaceholders';

const boom = new ApiError('Audit failed (500) [internal]: boom (request-id: r-9)', 500, 'internal', 'boom');

describe('queryView', () => {
  const empty = (d: string[]) => d.length === 0;
  test.each([
    [{ data: undefined, error: null, isPending: true }, 'loading'],
    [{ data: undefined, error: boom }, 'error'],
    [{ data: [], error: null }, 'empty'],
    [{ data: ['a'], error: null }, 'data'],
    // A failed background refetch keeps the rows that are on screen.
    [{ data: ['a'], error: boom }, 'data'],
    // Never 'empty' together with an error.
    [{ data: undefined, error: boom, isPending: false }, 'error'],
  ] as const)('%o → %s', (q, kind) => {
    expect(queryView<string[]>({ ...q, data: q.data as string[] | undefined }, empty).kind).toBe(kind);
  });
  test('several queries: one error wins over loading', () => {
    expect(queriesView([{ data: undefined, error: null }, { data: undefined, error: boom }]).kind).toBe('error');
    expect(queriesView([{ data: 1, error: null }, { data: undefined, error: null }]).kind).toBe('loading');
    expect(queriesView([{ data: 1, error: null }, { data: 'x', error: null }])).toEqual({ kind: 'data', data: [1, 'x'] });
  });
});

describe('QueryState', () => {
  test('an error shows ONE plain sentence, Retry and Details, and never the empty state', () => {
    const refetch = vi.fn();
    render(
      <QueryState
        query={{ data: undefined, error: boom, refetch }}
        what="the audit log"
        isEmpty={(d: string[]) => d.length === 0}
        empty={<EmptyState title="No audit entries yet" />}
      >
        {() => <div>rows</div>}
      </QueryState>,
    );
    expect(screen.getByRole('alert')).toHaveTextContent('Could not load the audit log');
    expect(screen.queryByText('No audit entries yet')).toBeNull();
    // The wire text is behind Details only.
    expect(screen.queryByText(/boom|500|request-id/)).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Details' }));
    expect(screen.getByText(/request-id: r-9/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Retry/ }));
    expect(refetch).toHaveBeenCalledTimes(1);
  });
  test('loading shows a skeleton, not the empty state', () => {
    render(
      <QueryState query={{ data: undefined, error: null, isPending: true }} what="the users" empty={<EmptyState title="No users yet" />}>
        {() => <div>rows</div>}
      </QueryState>,
    );
    expect(screen.getByRole('status', { name: 'Loading the users' })).toBeInTheDocument();
    expect(screen.queryByText('No users yet')).toBeNull();
  });
  test('data renders the children', () => {
    render(
      <QueryState query={{ data: ['a', 'b'], error: null }} what="x" isEmpty={(d: string[]) => d.length === 0} empty={<span>none</span>}>
        {(d) => <div>{d.join(',')}</div>}
      </QueryState>,
    );
    expect(screen.getByText('a,b')).toBeInTheDocument();
  });
});
