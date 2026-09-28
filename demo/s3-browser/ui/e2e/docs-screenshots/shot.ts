/**
 * The docs screenshot model. A shot is DATA: where to go, what to do, what to
 * mark. The runner (docs-screenshots.spec.ts) captures every shot in the light
 * and the dark theme and draws the annotations as an SVG overlay just before
 * the capture.
 *
 * Add a shot: append an entry to the file of its docs area in `shots/`
 * (storage, access, jobs, observability, system, browser). The markdown then
 * references `/_/screenshots/<id>.webp` once; both renderers pick the
 * `<id>.light.webp` or `<id>.dark.webp` file for the active theme.
 */
import type { APIRequestContext, Locator, Page } from '@playwright/test';

/**
 * How to find an element, in the order of preference: a role and its
 * accessible name, a form label, a test id. `text` and `css` are escape
 * hatches for elements that have none of these; `css` takes any Playwright
 * selector, so `text="by rule" >> xpath=..` reaches the box around a text.
 * `within` scopes the search.
 * A target must match exactly one visible element, or the run fails.
 */
export type Target = (
  | { role: Parameters<Page['getByRole']>[0]; name?: string | RegExp; exact?: boolean }
  | { label: string | RegExp; exact?: boolean }
  | { testId: string }
  | { placeholder: string; exact?: boolean }
  | { text: string | RegExp; exact?: boolean }
  | { css: string }
) & { within?: Target; nth?: number };

/** An annotation target: one element, or the smallest box around several (a group of fields). */
export type MarkTarget = Target | { union: Target[] };

export interface Annotation {
  target: MarkTarget;
  /**
   * arrow: points at the target from outside (never over its label).
   * box: a highlight frame around the target.
   * callout: a numbered badge next to the target (for multi-step shots).
   */
  kind: 'arrow' | 'box' | 'callout';
  /** Badge text: the step number for a callout, or a badge at the arrow's tail. */
  label?: string;
  /** Where the arrow comes from, or where the badge sits. Default: the side with the most room. */
  side?: 'left' | 'right' | 'top' | 'bottom';
}

export interface Shot {
  /** File stem: `docs/screenshots/<id>.{light,dark}.webp`. Lower case and hyphens. */
  id: string;
  /** App route, for example `/_/admin/storage/backends`. */
  route: string;
  /** The alt text the doc should use: a full sentence that says what the annotation marks. */
  alt: string;
  /** Actions after the route loads: open a form, type, select. Must not depend on the theme. */
  setup?: (page: Page) => Promise<void>;
  /**
   * Undo what `setup` changed on the server, with the signed-in admin API.
   * Runs after each capture, also after a failure, so every later shot sees
   * the seed. Setup changes the server only when a shot needs a state that
   * the seed cannot hold (the seed state is what the other shots show).
   */
  teardown?: (api: APIRequestContext) => Promise<void>;
  annotations?: Annotation[];
  /** Crop to this element or group (plus `clipPadding` CSS px). Default: the whole viewport. */
  clip?: MarkTarget;
  clipPadding?: number;
  /** Extra volatile regions to paint over (server times, live numbers). */
  mask?: Target[];
  /** CSS viewport; default 1280x800 at device scale 2. */
  viewport?: { width: number; height: number };
  /** `none`: capture signed out. Default: signed in as the bootstrap admin. */
  auth?: 'admin' | 'none';
}

export function resolve(page: Page | Locator, t: Target): Locator {
  const root = t.within ? resolve(page, t.within) : page;
  let loc: Locator;
  if ('role' in t) loc = root.getByRole(t.role, { name: t.name, exact: t.exact });
  else if ('label' in t) loc = root.getByLabel(t.label, { exact: t.exact });
  else if ('testId' in t) loc = root.getByTestId(t.testId);
  else if ('placeholder' in t) loc = root.getByPlaceholder(t.placeholder, { exact: t.exact });
  else if ('text' in t) loc = root.getByText(t.text, { exact: t.exact });
  else loc = root.locator(t.css);
  return t.nth === undefined ? loc : loc.nth(t.nth);
}

export function describeTarget(t: MarkTarget): string {
  if ('union' in t) return `union of ${t.union.map(describeTarget).join(', ')}`;
  const { within, ...rest } = t;
  return JSON.stringify(rest) + (within ? ` within ${describeTarget(within)}` : '');
}
