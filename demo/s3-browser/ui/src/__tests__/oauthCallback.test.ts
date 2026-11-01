import assert from 'node:assert/strict';
import { test } from 'vitest';
import { oauthCallbackUrl } from '../oauthCallback';

test('a pinned callback URL wins over the page origin', () => {
  assert.equal(
    oauthCallbackUrl('https://s3.acme.example/_/api/admin/oauth/callback', 'http://10.0.0.5:9000'),
    'https://s3.acme.example/_/api/admin/oauth/callback',
  );
});

test('without public_url the callback URL follows the page origin', () => {
  for (const pinned of [undefined, null, '']) {
    assert.equal(
      oauthCallbackUrl(pinned, 'https://s3.acme.example'),
      'https://s3.acme.example/_/api/admin/oauth/callback',
    );
  }
});
