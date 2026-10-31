/**
 * THE decision a data view makes before it renders (UX harvest D16, D17):
 * loading, error, empty or data — exactly ONE of them. Pure, so the truth
 * table is unit-tested (`queryView.test.ts`); <QueryState> renders it.
 *
 * A react-query result fits `QueryLike` as it is. A hand-rolled
 * `useState` loader passes `{ data, error, isPending: loading }`.
 */
export interface QueryLike<T> {
  data: T | undefined;
  error: unknown;
  /** True until the first answer (react-query `isPending`). */
  isPending?: boolean;
  /** react-query `isLoading`; read when `isPending` is not given. */
  isLoading?: boolean;
  refetch?: () => unknown;
}

type QueryViewKind = 'loading' | 'error' | 'empty' | 'data';

export interface QueryView<T> {
  kind: QueryViewKind;
  /** The data of every query, in order; set for 'empty' and 'data'. */
  data?: T;
  /** The first error, for 'error'. */
  error?: unknown;
}

/**
 * Pure: the view of one query.
 *
 *  - data present → 'data' (or 'empty' when `isEmpty(data)`), even while a
 *    background refetch runs or fails: stale rows beat a blank page.
 *  - no data, an error → 'error'. Never 'empty' with an error: "No users
 *    yet" under a failed request is a lie.
 *  - no data, no error → 'loading'.
 */
export function queryView<T>(query: QueryLike<T>, isEmpty?: (data: T) => boolean): QueryView<T> {
  if (query.data !== undefined && query.data !== null) {
    return { kind: isEmpty?.(query.data) ? 'empty' : 'data', data: query.data };
  }
  if (query.error) return { kind: 'error', error: query.error };
  return { kind: 'loading' };
}

/**
 * Pure: the view of a page that needs SEVERAL queries. Any query without
 * data and with an error → ONE 'error' (the first); any query still without
 * data → 'loading'; else the data of all of them, as a tuple.
 */
export function queriesView<T extends readonly unknown[]>(
  queries: { [K in keyof T]: QueryLike<T[K]> },
  isEmpty?: (data: T) => boolean,
): QueryView<T> {
  const failed = queries.find((q) => (q.data === undefined || q.data === null) && q.error);
  if (failed) return { kind: 'error', error: failed.error };
  if (queries.some((q) => q.data === undefined || q.data === null)) return { kind: 'loading' };
  const data = queries.map((q) => q.data) as unknown as T;
  return { kind: isEmpty?.(data) ? 'empty' : 'data', data };
}
