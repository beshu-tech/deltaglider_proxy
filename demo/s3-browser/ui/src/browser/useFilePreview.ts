import { useState, useEffect, useCallback } from 'react';
import { buildBrowserUrl, type BrowserLocation } from '../urlState';
import { useOverlayClose } from '../hooks/useOverlayClose';
import type { S3Object } from '../types';
import type { NavigateUrl } from './useBrowserLocation';

/**
 * FilePreview deep-link (?object=…&preview=1). Opening the preview pushes
 * ?preview=1 alongside ?object=… so the overlay is deep-linkable,
 * reload-safe, and Back closes it without over-navigating out of the folder.
 */
export function useFilePreview(browser: BrowserLocation, navigate: NavigateUrl, objects: S3Object[]) {
  const [previewObject, setPreviewObject] = useState<S3Object | null>(null);
  const { markPushed, closeOverlay } = useOverlayClose();

  const openPreview = useCallback(
    (obj: S3Object) => {
      setPreviewObject(obj);
      navigate(buildBrowserUrl({ ...browser, object: obj.key, preview: '1' }));
      markPushed();
    },
    [browser, navigate, markPushed],
  );

  // Close: remove preview=1 but keep ?object=… so the inspector stays open.
  const closePreview = useCallback(() => {
    const key = previewObject?.key ?? browser.object;
    closeOverlay(buildBrowserUrl({ ...browser, object: key, preview: '' }), navigate);
    setPreviewObject(null);
  }, [browser, previewObject, navigate, closeOverlay]);

  // Sync preview state with the URL: open on direct-load with ?preview=1,
  // close when Back / navigation removes the flag.
  useEffect(() => {
    if (browser.preview === '1' && browser.object && !previewObject) {
      const obj = objects.find((o) => o.key === browser.object);
      if (obj) setPreviewObject(obj);
    }
    if (browser.preview !== '1' && previewObject) {
      setPreviewObject(null);
    }
  }, [browser.preview, browser.object, previewObject, objects]);

  /** Drop the preview without touching the URL (folder or bucket change). */
  const clearPreview = useCallback(() => setPreviewObject(null), []);

  return { previewObject, openPreview, closePreview, clearPreview };
}
