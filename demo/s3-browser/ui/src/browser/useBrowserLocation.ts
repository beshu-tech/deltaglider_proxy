import { useState, useEffect, useLayoutEffect, useCallback, useRef } from 'react';
import { getBucket, setBucket } from '../s3client';
import { buildBrowserUrl } from '../urlState';
import { useOverlayClose } from '../hooks/useOverlayClose';

/** Low-level URL navigation from the router (push by default, {replace} to swap). */
export type NavigateUrl = (url: string, opts?: { replace?: boolean }) => void;

interface LocationOptions {
  /**
   * The browser location, derived from the URL by `useUrlRouter().browser`.
   * The URL is the single source of truth for where-am-I: bucket / current
   * folder prefix / search filter / open inspector object. Folder navigation
   * therefore creates real history entries (Back/Forward work) and reload
   * restores the folder.
   */
  bucket: string;
  prefix: string;
  q: string;
  object: string;
  navigateUrl: NavigateUrl;
}

/**
 * Where the browser is and how to move: folder and bucket navigation, the
 * search box (debounced into ?q=) and the inspector (?object=).
 */
export function useBrowserLocation({ bucket, prefix, q, object, navigateUrl }: LocationOptions) {
  // The search box shows a local draft; the URL's ?q= follows 200ms later
  // (debounced replace, see setSearchQuery). Binding the box to ?q= directly
  // made each keystroke edit the value from BEFORE the previous one, so at
  // normal typing speed characters vanished ("09-03" → "3"). An external ?q=
  // change (Back, a link) resets the draft unless the user is mid-typing.
  const qDebounce = useRef<number | null>(null);
  const [draftQ, setDraftQ] = useState(q);
  useEffect(() => {
    if (qDebounce.current === null) setDraftQ(q);
  }, [q]);
  const prefixRef = useRef(prefix);
  prefixRef.current = prefix;
  // bucket/object tracked in refs too so the debounced search write reads the
  // LATEST values when its timer fires, not the values captured when the
  // keystroke was typed (see setSearchQuery).
  const bucketRef = useRef(bucket);
  bucketRef.current = bucket;
  const objectRef = useRef(object);
  objectRef.current = object;

  // Keep the s3client's module-level active bucket in sync with the URL bucket
  // (the AWS-SDK calls read it). A LAYOUT effect: it runs before every passive
  // effect, the children's included, so neither the list fetch nor a child's
  // first request (the inspector's HEAD on a ?object= deep link) sees the
  // previous bucket.
  useLayoutEffect(() => {
    if (bucket && bucket !== getBucket()) {
      setBucket(bucket);
    }
  }, [bucket]);

  // Direct-load-safe close for the inspector overlay (Tier 1.6 fix).
  const { markPushed: markInspectorPushed, closeOverlay: closeInspectorOverlay } = useOverlayClose();

  const openInspector = useCallback((key: string) => {
    // Fall back to the s3client's active bucket if the URL-derived bucket is
    // still empty (e.g. landed at /_/browse before the URL gained the bucket
    // segment). buildBrowserUrl drops everything when bucket is empty, which
    // would otherwise no-op the navigation.
    navigateUrl(buildBrowserUrl({ bucket: bucket || getBucket(), prefix, q, object: key }));
    markInspectorPushed();
  }, [navigateUrl, bucket, prefix, q, markInspectorPushed]);

  const closeInspector = useCallback(() => {
    if (!object) return;
    // Direct-load-safe: if we pushed the ?object= entry, Back pops it cleanly.
    // If the page was loaded with ?object= already present (shared link),
    // replace the URL to drop it instead of walking out of the SPA.
    closeInspectorOverlay(
      buildBrowserUrl({ bucket: bucket || getBucket(), prefix, q }),
      navigateUrl,
    );
  }, [object, closeInspectorOverlay, navigateUrl, bucket, prefix, q]);

  // Folder navigation: PUSH a new history entry for the new prefix (same bucket,
  // drop any active search). Back returns to the parent folder. The URL change
  // re-derives prefix and triggers the listing's reset + list effects.
  const navigate = useCallback((newPrefix: string) => {
    // Fall back to the s3client's active bucket when the URL-derived bucket is
    // empty. At bare /_/browse, `bucket` is '' until the URL gains the segment;
    // buildBrowserUrl({ bucket: '', ... }) drops the prefix and yields /_/browse,
    // so a folder click would silently no-op (the v1.3.1 "folders don't open" bug).
    navigateUrl(buildBrowserUrl({ bucket: bucket || getBucket(), prefix: newPrefix }));
  }, [navigateUrl, bucket]);

  // Bucket switch: PUSH; prefix + search reset to root of the new bucket.
  const changeBucket = useCallback((newBucket: string) => {
    setBucket(newBucket); // keep s3client in sync immediately (the effect also does this)
    navigateUrl(buildBrowserUrl({ bucket: newBucket }));
  }, [navigateUrl]);

  // Search filter lives in the URL as ?q=, written as a debounced REPLACE so
  // typing doesn't spam the history stack (each keystroke swaps the current
  // entry rather than pushing). REPLACE also means Back from a filtered view
  // leaves the folder rather than undoing keystrokes.
  const setSearchQuery = useCallback((next: string) => {
    setDraftQ(next);
    if (qDebounce.current !== null) window.clearTimeout(qDebounce.current);
    qDebounce.current = window.setTimeout(() => {
      qDebounce.current = null;
      // Read bucket/prefix/object from refs (latest values), not from the
      // closure captured when the keystroke was typed. Otherwise a bucket/
      // prefix change during the 200ms window would write a stale URL — and if
      // the captured bucket was '' (at bare /_/browse), buildBrowserUrl would
      // drop the prefix entirely. Fall back to getBucket() for the same reason
      // navigate() does.
      navigateUrl(
        buildBrowserUrl({
          bucket: bucketRef.current || getBucket(),
          prefix: prefixRef.current,
          q: next,
          object: objectRef.current,
        }),
        { replace: true },
      );
    }, 200);
  }, [navigateUrl]);
  useEffect(() => () => {
    if (qDebounce.current !== null) window.clearTimeout(qDebounce.current);
  }, []);

  return {
    searchQuery: draftQ,
    setSearchQuery,
    navigate,
    changeBucket,
    openInspector,
    closeInspector,
  };
}
