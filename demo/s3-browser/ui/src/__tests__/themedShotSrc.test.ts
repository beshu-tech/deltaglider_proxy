import { describe, expect, test } from 'vitest';
import { themedShotSrc } from '../docsMarkdown';

describe('themedShotSrc', () => {
  test('a theme-neutral pipeline shot picks the variant of the active theme', () => {
    expect(themedShotSrc('/_/screenshots/route-bucket-backend-form.webp', false)).toBe(
      '/_/screenshots/route-bucket-backend-form.light.webp',
    );
    expect(themedShotSrc('/_/screenshots/route-bucket-backend-form.webp', true)).toBe(
      '/_/screenshots/route-bucket-backend-form.dark.webp',
    );
  });

  test('other images pass through unchanged', () => {
    for (const src of [
      '/_/screenshots/storage_backends.jpg',
      '/_/screenshots/x.light.webp',
      '/_/screenshots/x.dark.webp',
      'https://example.com/a.webp',
      '/_/screenshots/../secret.webp',
      '',
    ]) {
      expect(themedShotSrc(src, true)).toBe(src);
    }
    expect(themedShotSrc(undefined, false)).toBeUndefined();
  });
});
