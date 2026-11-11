/**
 * WCAG contrast maths for the contrast tests, strict on its input.
 *
 * The first version sliced `#rrggbb` by position: `#777`, `rgb(...)` and
 * `hsl(...)` gave NaN, `NaN < 4.5` is false, so a token in any other format
 * passed every contrast test. Now an unparseable colour THROWS, and a ratio
 * that is not a number counts as a failure (`meetsAA` is `r >= 4.5`, never
 * the negation of `r < 4.5`).
 */

export type Rgba = [number, number, number, number];

/** `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb(r, g, b)`, `rgba(r, g, b, a)`. Anything else throws. */
export function parseColor(c: string): Rgba {
  const rgb = /^rgba?\(([^)]+)\)$/.exec(c.trim());
  if (rgb) {
    const parts = rgb[1].split(',').map((s) => s.trim());
    if (parts.length < 3 || parts.length > 4) throw new Error(`unparseable colour: ${c}`);
    const [r, g, b, a = '1'] = parts;
    const out: Rgba = [Number(r), Number(g), Number(b), Number(a)];
    if (out.some((n) => !Number.isFinite(n))) throw new Error(`unparseable colour: ${c}`);
    return out;
  }
  const hex = /^#([0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.exec(c.trim());
  if (!hex) throw new Error(`unparseable colour (want #rgb, #rrggbb or rgb()): ${c}`);
  const h = hex[1];
  const full = h.length <= 4 ? [...h].map((x) => x + x).join('') : h;
  const ch = (i: number) => parseInt(full.slice(i, i + 2), 16);
  return [ch(0), ch(2), ch(4), full.length === 8 ? ch(6) / 255 : 1];
}

function luminance(color: string): number {
  const [r, g, b, a] = parseColor(color);
  // Contrast of a translucent colour depends on what is behind it; reading
  // only r/g/b would measure it as opaque and pass it.
  if (a < 1) throw new Error(`unparseable colour: translucent, flatten() it first: ${color}`);
  return rel([r, g, b]);
}

function rel(rgb: number[]): number {
  const [r, g, b] = rgb
    .map((v) => v / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function ratio(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

/** True only for a real number >= 4.5. NaN is NOT a pass. */
export function meetsAA(r: number): boolean {
  return r >= 4.5;
}

/** Alpha-composite `fg` over `bg` into `#rrggbb`. */
export function flatten(fg: string, bg: string): string {
  const [r, g, b, a] = parseColor(fg);
  const [R, G, B] = parseColor(bg);
  return (
    '#' +
    [[r, R], [g, G], [b, B]]
      .map(([x, y]) => Math.round(x * a + y * (1 - a)).toString(16).padStart(2, '0'))
      .join('')
  );
}
