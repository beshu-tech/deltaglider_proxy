import type { View } from './urlState';

const APP = 'DeltaGlider Proxy';

/**
 * The browser-tab title for a view. Pure so the unit test can pin
 * it: the old inline table read the bucket once per view change, so a fresh
 * load (no bucket yet) showed "— DeltaGlider Proxy" and a bucket switch
 * never updated the tab.
 */
export function pageTitle(view: View, bucket: string, adminPageLabel?: string): string {
  switch (view) {
    case 'browser':
      return bucket ? `${bucket} — ${APP}` : `Browse — ${APP}`;
    case 'upload':
      return bucket ? `Upload to ${bucket} — ${APP}` : `Upload — ${APP}`;
    case 'docs':
      return `Docs — ${APP}`;
    case 'admin':
      return adminPageLabel ? `${adminPageLabel} · Settings — ${APP}` : `Settings — ${APP}`;
  }
}
