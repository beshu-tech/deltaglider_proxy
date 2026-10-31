/**
 * Every theme (style x mode) with its palette and AntD theme, for the tests
 * that must hold in all of them (contrast, hljs, charts, mermaid).
 */
import { PALETTES, type ColorTokens, type ThemeStyle } from '../ThemeContext';
import { antdTheme } from '../theme';

export interface ThemeCase {
  name: string;
  style: ThemeStyle;
  mode: 'dark' | 'light';
  colors: ColorTokens;
  antd: ReturnType<typeof antdTheme>;
}

export const ALL_THEMES: ThemeCase[] = (Object.keys(PALETTES) as ThemeStyle[]).flatMap((style) =>
  (['dark', 'light'] as const).map((mode) => ({
    name: `${style} ${mode}`,
    style,
    mode,
    colors: PALETTES[style][mode],
    antd: antdTheme(style, mode === 'dark'),
  })),
);
