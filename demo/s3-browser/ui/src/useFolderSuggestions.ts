/**
 * Existing-folder suggestions for the upload-destination autocomplete.
 *
 * Lists the bucket's real folders one level at a time (S3 delimiter listing
 * under the typed path's parent), debounced and cached per parent so moving
 * the cursor inside one segment never refires the request. Failures degrade
 * to "no suggestions" — the destination stays a free-text input either way
 * (new folders are created on upload).
 */
import { useEffect, useRef, useState } from 'react';
import { listCommonPrefixes } from './s3client';
import { splitDestinationInput, filterFolderOptions } from './destinationSuggest';
import { keyPathError } from './components/destPrefix';

const DEBOUNCE_MS = 200;

export function useFolderSuggestions(bucket: string, input: string): string[] {
  const [suggestions, setSuggestions] = useState<string[]>([]);
  // Per-parent cache for the lifetime of the page (folder sets change rarely
  // and only through this very upload flow).
  const cacheRef = useRef(new Map<string, string[]>());

  const { parent, tail } = splitDestinationInput(input);

  useEffect(() => {
    // A '.'/'..' parent is not a real folder, and the proxy refuses to list it.
    if (!bucket || keyPathError(parent) !== null) {
      setSuggestions([]);
      return;
    }
    let cancelled = false;

    const apply = (folders: string[]) => {
      if (!cancelled) setSuggestions(filterFolderOptions(folders, parent, tail));
    };

    const cached = cacheRef.current.get(parent);
    if (cached) {
      apply(cached);
      return;
    }

    const id = window.setTimeout(() => {
      listCommonPrefixes(bucket, parent)
        .then((folders) => {
          cacheRef.current.set(parent, folders);
          apply(folders);
        })
        .catch(() => apply([]));
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(id);
    };
  }, [bucket, parent, tail]);

  return suggestions;
}
