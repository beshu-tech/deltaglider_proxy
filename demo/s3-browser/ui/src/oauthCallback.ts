/**
 * The OAuth callback URL that the proxy sends to the identity provider.
 * `GET /config` reports `oauth_callback_url` when `advanced.public_url`
 * pins it; else the proxy builds it from the request's host, which is the
 * origin that this page loads from.
 */
export function oauthCallbackUrl(pinned: string | null | undefined, origin: string): string {
  return pinned || `${origin}/_/api/admin/oauth/callback`;
}
