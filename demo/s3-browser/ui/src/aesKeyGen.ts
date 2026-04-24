// Shared client-side AES-256 key generator. Used by the per-backend
// encryption subsection in BackendsPanel.
//
// Generates 32 bytes of CSPRNG output via `crypto.getRandomValues` —
// the browser's Web Crypto API. The key never round-trips through
// the server before Apply; the operator copies it to off-box storage
// first, then clicks Apply which persists the key via the section
// PUT.
//
// A pure function, not a hook. The file used to be named
// `useGenerateAesKey.ts` but that invited `react-hooks/rules-of-hooks`
// false positives and misled readers about the type of the export.

/**
 * Return a fresh 32-byte (256-bit) AES key as 64 lowercase hex chars.
 * Uses `crypto.getRandomValues` — the browser's CSPRNG. Entropy is
 * sourced from the OS; output is suitable for AES-256-GCM.
 *
 * The returned string is the exact shape `EncryptionKey::from_hex`
 * expects on the server — 64 hex digits, no whitespace, no prefix.
 */
export function generateAesKeyHex(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}
