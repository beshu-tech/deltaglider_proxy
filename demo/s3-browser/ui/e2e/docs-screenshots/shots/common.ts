import type { Target } from '../shot';

/** An entry of the admin sidebar, by its visible label (the icon adds a word in front of it). */
export const nav = (label: string): Target => ({
  role: 'button',
  name: new RegExp(`(^|\\s)${label}$`),
  within: { role: 'navigation', name: 'Admin navigation' },
});
