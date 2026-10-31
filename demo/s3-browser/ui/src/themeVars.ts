/**
 * THE bridge from the palette to CSS. `darkColors` / `lightColors`
 * (ThemeContext.tsx) are the one source of every theme colour; this module
 * publishes each token as a CSS custom property, so a stylesheet reads
 * `var(--dg-bg-card)` instead of copying `#111827` or having a component
 * inject `--my-border: colors.BORDER` inline.
 *
 *   BG_CARD → --dg-bg-card, TEXT_MUTED → --dg-text-muted, …
 *
 * The fonts live here too: FONT_UI / FONT_MONO feed AntD (theme.ts), mermaid
 * and `--font-ui` / `--font-mono`. The faces are bundled (@fontsource-variable,
 * imported by the entry points), never fetched from a CDN: the admin UI must
 * work on an air-gapped host and must not tell a third party who opens it.
 *
 * Pure (no CSS imports), so node unit tests can import it.
 */
import { darkColors, lightColors, type ColorTokens } from './ThemeContext';

export const FONT_UI = "'Manrope Variable', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";
export const FONT_MONO = "'JetBrains Mono Variable', ui-monospace, SFMono-Regular, Menlo, monospace";

/** Pure: the custom-property name of a palette token (`BG_CARD` → `--dg-bg-card`). */
export function cssVarName(token: keyof ColorTokens | string): string {
  return `--dg-${token.toLowerCase().replace(/_/g, '-')}`;
}

/** Pure: `var(--dg-…)` of a palette token, for inline styles that must follow the theme without a re-render. */
export function cssVar(token: keyof ColorTokens): string {
  return `var(${cssVarName(token)})`;
}

/**
 * Pure: `color` at `percent` % opacity, for a tint or a soft border. Works on
 * every colour form (hex, rgb(), var(--dg-…)); the old `${color}22` hex-alpha
 * concatenation made invalid CSS, silently, for anything but #rrggbb.
 */
export function tint(color: string, percent: number): string {
  return `color-mix(in srgb, ${color} ${percent}%, transparent)`;
}

/**
 * Pure: the tinted band above a listing (a running job, a notice): the
 * accent at 9 % behind, at 30 % as the bottom rule. CSS vars, so it follows
 * the theme without a hook.
 */
export function noticeBand(accent: 'ACCENT_BLUE' | 'ACCENT_AMBER' | 'ACCENT_RED' | 'ACCENT_GREEN') {
  return { background: tint(cssVar(accent), 9), borderBottom: `1px solid ${tint(cssVar(accent), 30)}` };
}

/** Pure: the declarations of one palette, one per token. */
export function paletteDeclarations(colors: ColorTokens): string {
  return Object.entries(colors)
    .map(([token, value]) => `${cssVarName(token)}: ${value};`)
    .join(' ');
}

/** Pure: the whole generated stylesheet (fonts + both palettes). */
export function themeStylesheet(): string {
  return [
    `:root { --font-ui: ${FONT_UI}; --font-mono: ${FONT_MONO}; }`,
    `[data-theme="dark"] { ${paletteDeclarations(darkColors)} }`,
    `[data-theme="light"] { ${paletteDeclarations(lightColors)} }`,
  ].join('\n');
}

const STYLE_ID = 'dg-theme-vars';

/** Adds the generated stylesheet to <head> once. Call before the first render. */
export function installThemeVars(doc: Document = document): void {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement('style');
  style.id = STYLE_ID;
  style.textContent = themeStylesheet();
  doc.head.prepend(style);
}
