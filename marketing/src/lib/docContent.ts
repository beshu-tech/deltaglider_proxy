// docContent.ts — load the raw markdown of every product doc at build time.
//
// The docs live OUTSIDE marketing/ (in the repo's docs/product/), so they're
// the same files the binary bundles — there is exactly one copy. Vite's
// import.meta.glob with `?raw` pulls them in as strings during SSG.
//
// Keyed by the docs/product path WITHOUT extension (e.g. "auth/30-oauth-setup"),
// matching the manifest `path` so docs.ts/renderDoc.ts can look content up by it.

const rawModules = import.meta.glob('../../../docs/product/**/*.md', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

const CONTENT_BY_PATH: Record<string, string> = {};
for (const [absPath, content] of Object.entries(rawModules)) {
  // absPath looks like "../../../docs/product/auth/30-oauth-setup.md"
  const m = absPath.match(/\/docs\/product\/(.+)\.md$/);
  if (m) CONTENT_BY_PATH[m[1]] = content;
}

export function docContent(path: string): string | undefined {
  return CONTENT_BY_PATH[path];
}
