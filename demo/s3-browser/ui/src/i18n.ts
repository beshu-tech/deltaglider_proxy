/**
 * i18n.ts — Wave 10.1 §10.2 scaffold.
 *
 * A pass-through `t()` helper today. Purpose: when we actually ship
 * a second locale, we swap this one file's internals (load dict,
 * lookup, fallback) without touching every component. Callers just
 * use `t('some.key')` (or `t('some.key', 'Fallback English')`) and
 * the shape never changes.
 *
 * Design choices:
 *
 *   - **Dotted string keys, not message objects.** Translators work
 *     in flat key→string catalogs; React components read by key.
 *   - **Fallback-first.** Every call site passes the English string
 *     as the *key* itself OR as the explicit fallback. Today the
 *     fallback *is* the return value. A missing-key call is
 *     impossible because there is no lookup table yet.
 *   - **No React dependency.** This is a plain function so it
 *     works in loader files, utilities, etc. The `useT()` hook
 *     exists for consistency — today it returns the same `t`.
 *
 * When we add a real locale, the implementation becomes:
 *
 *     const DICTS: Record<string, Record<string,string>> = { ... };
 *     function t(key, fallback) {
 *       const locale = currentLocale();
 *       return DICTS[locale]?.[key] ?? fallback ?? key;
 *     }
 *
 * …and every existing call site continues to work.
 */

/**
 * Translate a message. Today: returns `fallback ?? key` (pass-through
 * so wiring this everywhere is a no-op).
 *
 * @param key       Dotted key used to look up translated strings
 *                  once we add a locale catalog (e.g. `admin.apply.button`).
 * @param fallback  English default. Also the string we return today.
 *                  Keeping this explicit so future translators can
 *                  see the original intent inline with the call site.
 */
export function t(key: string, fallback?: string): string {
  return fallback ?? key;
}

/**
 * React hook form. Returns the `t` function. The only reason this
 * exists is to leave a stable surface for when `t()` has to read
 * locale state — at that point the hook subscribes to locale
 * changes; today it's a trivial wrapper.
 *
 * Example:
 *
 *     const t = useT();
 *     return <button>{t('admin.apply.button', 'Apply')}</button>;
 */
export function useT(): typeof t {
  return t;
}
