/**
 * Draws a shot's annotations as one SVG overlay over the viewport, from the
 * bounding boxes of their targets. The geometry is computed here; the page
 * only receives the finished SVG.
 *
 * Readability in both themes: every mark is drawn twice, a wide stroke in the
 * page background colour (a halo) under a narrow stroke in the brand colour
 * (`--focus-ring`, the accent token of the active theme). An arrow ends
 * outside its target and a callout badge sits outside it, so neither covers
 * the label of the control that it marks.
 */
import type { Page } from '@playwright/test';
import { describeTarget, resolve, type Annotation, type MarkTarget, type Target } from './shot';

interface Box { x: number; y: number; width: number; height: number }
type Side = 'left' | 'right' | 'top' | 'bottom';

const OVERLAY_ID = 'dg-shot-overlay';
const PAD = 6; // box frame distance from the target
const GAP = 8; // arrow tip distance from the target
const ARROW = 84; // arrow length
const R = 13; // badge radius

/** The side of `b` with the most room in a `vw` x `vh` viewport; `prefer` wins when it has room for `need`. */
function pickSide(b: Box, vw: number, vh: number, need: number, prefer?: Side): Side {
  const room: Record<Side, number> = {
    left: b.x,
    right: vw - (b.x + b.width),
    top: b.y,
    bottom: vh - (b.y + b.height),
  };
  if (prefer && room[prefer] >= need) return prefer;
  // Horizontal arrows read best next to form controls; use them when they fit.
  if (Math.max(room.left, room.right) >= need) return room.left >= room.right ? 'left' : 'right';
  return room.top >= room.bottom ? 'top' : 'bottom';
}

function badge(cx: number, cy: number, text: string): string {
  return (
    `<circle cx="${cx}" cy="${cy}" r="${R + 3}" fill="var(--halo)"/>` +
    `<circle cx="${cx}" cy="${cy}" r="${R}" fill="var(--mark)"/>` +
    `<text x="${cx}" y="${cy}" dy="0.35em" text-anchor="middle" font-family="Manrope, sans-serif" ` +
    `font-weight="800" font-size="15" fill="var(--halo)">${text}</text>`
  );
}

function stroked(shape: string): string {
  // `shape` has no stroke attributes; draw the halo, then the mark.
  return (
    shape.replace('/>', ' fill="none" stroke="var(--halo)" stroke-width="8" stroke-linecap="round" stroke-linejoin="round"/>') +
    shape.replace('/>', ' fill="none" stroke="var(--mark)" stroke-width="3.5" stroke-linecap="round" stroke-linejoin="round"/>')
  );
}

/** Pure: the SVG body for the annotations on their resolved boxes. */
export function annotationSvg(items: { a: Annotation; b: Box }[], vw: number, vh: number): string {
  let out = '';
  for (const { a, b } of items) {
    if (a.kind === 'box') {
      out += stroked(
        `<rect x="${b.x - PAD}" y="${b.y - PAD}" width="${b.width + 2 * PAD}" height="${b.height + 2 * PAD}" rx="8"/>`,
      );
      if (a.label) {
        const side = pickSide(b, vw, vh, 2 * R + PAD + 8, a.side ?? 'left');
        const [cx, cy] = badgeAt(b, side);
        out += badge(cx, cy, a.label);
      }
    } else if (a.kind === 'callout') {
      const side = pickSide(b, vw, vh, 2 * R + PAD + 8, a.side ?? 'left');
      const [cx, cy] = badgeAt(b, side);
      out += badge(cx, cy, a.label ?? '1');
    } else {
      const side = pickSide(b, vw, vh, ARROW + GAP + 2 * R, a.side);
      const cx = b.x + b.width / 2;
      const cy = b.y + b.height / 2;
      // Tip: the middle of the chosen edge, GAP outside. Tail: outward and a
      // little diagonal, the way a hand-drawn arrow comes in.
      const [tx, ty, dx, dy] =
        side === 'left' ? [b.x - GAP, cy, -1, 0]
        : side === 'right' ? [b.x + b.width + GAP, cy, 1, 0]
        : side === 'top' ? [cx, b.y - GAP, 0, -1]
        : [cx, b.y + b.height + GAP, 0, 1];
      const len = ARROW;
      const px = dy === 0 ? 0 : 0.5; // perpendicular drift
      const py = dx === 0 ? 0 : 0.5;
      const ex = tx + dx * len + px * len * (tx > vw / 2 ? -1 : 1);
      const ey = ty + dy * len + py * len * (ty > vh / 2 ? -1 : 1);
      // Arrowhead: two short strokes back from the tip.
      const ang = Math.atan2(ey - ty, ex - tx);
      const h = 14;
      const spread = 0.5;
      const h1 = [tx + h * Math.cos(ang + spread), ty + h * Math.sin(ang + spread)];
      const h2 = [tx + h * Math.cos(ang - spread), ty + h * Math.sin(ang - spread)];
      out += stroked(`<path d="M${ex},${ey} L${tx},${ty} M${h1[0]},${h1[1]} L${tx},${ty} L${h2[0]},${h2[1]}"/>`);
      if (a.label) {
        const bx = ex + Math.cos(ang) * (R + 2);
        const by = ey + Math.sin(ang) * (R + 2);
        out += badge(bx, by, a.label);
      }
    }
  }
  return out;
}

function badgeAt(b: Box, side: Side): [number, number] {
  const off = PAD + R + 6;
  if (side === 'left') return [b.x - off, b.y + b.height / 2];
  if (side === 'right') return [b.x + b.width + off, b.y + b.height / 2];
  if (side === 'top') return [b.x, b.y - off];
  return [b.x, b.y + b.height + off];
}

/** The box of one target: exactly one visible element, or throw. */
async function targetBox(page: Page, t: Target): Promise<Box> {
  const loc = resolve(page, t);
  const n = await loc.count();
  if (n !== 1) throw new Error(`annotation target ${describeTarget(t)} matches ${n} elements (want exactly 1)`);
  await loc.waitFor({ state: 'visible', timeout: 10_000 });
  // A box read during a modal's zoom-in or a panel's expand is wrong by the
  // time of the capture: read until two reads agree.
  let b = await loc.boundingBox();
  for (let i = 0; i < 40; i++) {
    await page.waitForTimeout(50);
    const next = await loc.boundingBox();
    if (b && next && JSON.stringify(b) === JSON.stringify(next)) break;
    b = next;
  }
  if (!b || b.width === 0 || b.height === 0) throw new Error(`annotation target ${describeTarget(t)} has no visible box`);
  return b;
}

/** Wait until no CSS animation or transition runs (AntD motion on open and close). */
export async function waitForMotion(page: Page): Promise<void> {
  await page
    .waitForFunction(() => document.getAnimations().every((a) => a.playState !== 'running'), undefined, { timeout: 5_000 })
    .catch(() => undefined); // an endless animation (a spinner) must not block the shot
}

export async function markBox(page: Page, t: MarkTarget): Promise<Box> {
  if (!('union' in t)) return targetBox(page, t);
  const boxes = await Promise.all(t.union.map((u) => targetBox(page, u)));
  const x = Math.min(...boxes.map((b) => b.x));
  const y = Math.min(...boxes.map((b) => b.y));
  return {
    x,
    y,
    width: Math.max(...boxes.map((b) => b.x + b.width)) - x,
    height: Math.max(...boxes.map((b) => b.y + b.height)) - y,
  };
}

/**
 * Resolve every annotation target (exactly one visible element each, or
 * throw), then inject the overlay. Returns the boxes, so the caller can
 * check that the marks sit inside the captured area.
 */
export async function drawAnnotations(page: Page, annotations: Annotation[]): Promise<Box[]> {
  await waitForMotion(page);
  const items: { a: Annotation; b: Box }[] = [];
  for (const a of annotations) {
    const b = await markBox(page, a.target);
    items.push({ a, b });
  }
  const vp = page.viewportSize() ?? { width: 1280, height: 800 };
  for (const { a, b } of items) {
    if (b.x < 0 || b.y < 0 || b.x + b.width > vp.width || b.y + b.height > vp.height) {
      throw new Error(`annotation target ${describeTarget(a.target)} is outside the viewport: ${JSON.stringify(b)}`);
    }
  }
  const body = annotationSvg(items, vp.width, vp.height);
  await page.evaluate(
    ({ id, body, w, h }) => {
      document.getElementById(id)?.remove();
      const root = getComputedStyle(document.documentElement);
      const mark = root.getPropertyValue('--focus-ring').trim() || '#0f766e';
      const dark = document.documentElement.dataset.theme === 'dark';
      const bg = getComputedStyle(document.body).backgroundColor;
      const halo = bg && !bg.startsWith('rgba(0, 0, 0, 0)') && bg !== 'transparent' ? bg : dark ? '#0b1120' : '#ffffff';
      const wrap = document.createElement('div');
      wrap.id = id;
      wrap.style.cssText = `position:fixed;inset:0;z-index:2147483647;pointer-events:none;--mark:${mark};--halo:${halo}`;
      wrap.innerHTML = `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}">${body}</svg>`;
      document.body.appendChild(wrap);
    },
    { id: OVERLAY_ID, body, w: vp.width, h: vp.height },
  );
  return items.map((i) => i.b);
}

export async function clearAnnotations(page: Page): Promise<void> {
  await page.evaluate((id) => document.getElementById(id)?.remove(), OVERLAY_ID);
}
