/**
 * The rule filter preview: the server runs the rules' own matcher on a
 * sample of the bucket. Debounced, so typing a glob sends one request when
 * the typing pauses, and the previous answer stays on screen meanwhile.
 */
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { previewFilters } from '../adminApi/jobs';
import { useDebounced } from '../hooks/useDebounced';

const DEBOUNCE_MS = 300;

export function useFilterPreview(args: {
  bucket: string;
  prefix: string;
  include: string[];
  exclude: string[];
  /** Classify these keys instead of the sample ("test a key"). */
  keys?: string[];
  enabled?: boolean;
}) {
  const debounced = useDebounced(
    JSON.stringify([args.bucket, args.prefix, args.include, args.exclude, args.keys ?? null]),
    DEBOUNCE_MS,
  );
  const [bucket, prefix, include, exclude, keys] = JSON.parse(debounced) as [
    string,
    string,
    string[],
    string[],
    string[] | null,
  ];
  return useQuery({
    queryKey: ['filter-preview', debounced],
    queryFn: () =>
      previewFilters({
        bucket,
        prefix,
        include_globs: include,
        exclude_globs: exclude,
        ...(keys ? { keys } : {}),
      }),
    enabled: (args.enabled ?? true) && bucket.length > 0 && (keys === null || keys.length > 0),
    placeholderData: keepPreviousData,
    retry: false,
    staleTime: 15_000,
  });
}
