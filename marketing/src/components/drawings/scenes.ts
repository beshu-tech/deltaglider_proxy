// scenes — one hand-written line drawing of the site's drawing set.
// Grammar (docs/plan/site-rewrite-2026-11.md §3 "Look"): one 2 px ink
// stroke, one file shape in every scene, and the brand colour only for
// the thing the drawing is about (a delta, a check, a key). No text
// inside the SVG: the labels are HTML next to it, so they stay readable
// on a phone and follow the theme.
//
// Classes: k = ink stroke, s = soft detail stroke, c = fill that hides
// what is behind (page background), t = tinted fill, b = brand stroke,
// bf = brand fill, bt = brand tint, h = ink fill (hash blocks).
export type SceneName =
  | 'bucket' | 'rebuild' | 'check' | 'client'
  | 'upload' | 'sort' | 'encode' | 'store'
  | 'key' | 'lock' | 'cipher-bucket' | 'proxy';

const FILE = (x: number, y: number, w: number, h: number, c: number, cls = 'k t') =>
  `<path class="${cls}" d="M${x} ${y}h${w - c}l${c} ${c}v${h - c}h${-w}z"/><path class="k" d="M${x + w - c} ${y}v${c}h${c}"/>`;
const LINES = (x: number, y: number, w: number, n = 3, gap = 9) =>
  `<path class="s" d="${Array.from({ length: n }, (_, i) => `M${x} ${y + i * gap}h${w}`).join('')}"/>`;
const ARROW = (x: number, y: number) =>
  `<path class="s" d="M${x} ${y}h8M${x + 5} ${y - 4}l4 4l-4 4"/>`;
const BUCKET_BACK = `<ellipse class="k c" cx="80" cy="52" rx="62" ry="12"/>`;
const BUCKET_FRONT =
  `<path class="k c" d="M18 52A62 12 0 0 0 142 52L131 114A51 10 0 0 1 29 114Z"/><path class="s" d="M21.500 72A59 11 0 0 0 138.500 72"/>`;
const HASH = (y: number, cls = 'h') => {
  const o = [1, 0.3, 0.65, 0.3, 1, 0.65, 0.3, 1];
  return `<g class="${cls}">${o.map((v, i) => `<rect x="${22 + i * 15}" y="${y}" width="12" height="12" rx="2" opacity="${v}"/>`).join('')}</g>`;
};
const CHIP =
  `<path class="k" d="M76 40v-7M86 40v-7M96 40v-7M76 97v-7M86 97v-7M96 97v-7"/><rect class="k c" x="66" y="40" width="40" height="50" rx="7"/><path class="b" d="M86 53l10 20h-20z"/>`;
const LAPTOP = `<rect class="k c" x="26" y="16" width="108" height="76" rx="7"/><path class="k c" d="M10 100h140l-9 11h-122z"/>`;

export const SCENES: Record<SceneName, string> = {
  // A bucket with the reference file and one thin delta in it.
  bucket: BUCKET_BACK + FILE(44, 6, 50, 66, 12) + LINES(52, 30, 32, 3, 10) +
    `<rect class="bf" x="104" y="20" width="7" height="50" rx="2"/>` + BUCKET_FRONT,
  // reference + delta -> xdelta3 -> the whole file.
  rebuild: FILE(8, 38, 28, 46, 8) + `<rect class="bf" x="42" y="46" width="5" height="38" rx="1.500"/>` +
    ARROW(52, 65) + CHIP + ARROW(110, 65) + FILE(124, 34, 28, 56, 10, 'k c') + LINES(130, 56, 16),
  // Two equal hash strips, then the gate.
  check: HASH(10) + HASH(30) + `<path class="s" d="M80 47v9"/>` +
    `<path class="b bt" d="M80 60l27 9v19c0 16-11 28-27 34c-16-6-27-18-27-34v-19z"/><path class="b" d="M67 91l10 10l17-19"/>`,
  // A laptop that shows the whole file.
  client: LAPTOP + FILE(66, 28, 28, 52, 10) + LINES(72, 50, 16),
  // A laptop that sends the file up.
  upload: LAPTOP + FILE(50, 28, 28, 52, 10) + LINES(56, 50, 16) +
    `<path class="s" d="M106 76v-40M96 46l10-10l10 10"/>`,
  // One file, two tracks: delta candidates up, the rest straight to storage.
  sort: FILE(10, 40, 30, 50, 10) + LINES(16, 62, 18) +
    `<path class="k" d="M48 65h22"/><circle class="k c" cx="78" cy="65" r="7"/>` +
    `<path class="b" d="M84 60c14-10 22-22 40-22h22M139 31l8 7l-8 7"/>` +
    `<path class="s" d="M84 70c14 10 22 22 40 22h22M139 85l8 7l-8 7"/>`,
  // reference + the new file -> xdelta3 -> a thin delta.
  encode: FILE(4, 30, 26, 44, 8) + FILE(22, 46, 28, 46, 8, 'k c') + LINES(28, 66, 16, 2) +
    ARROW(54, 65) + CHIP + ARROW(110, 65) + `<rect class="bf" x="130" y="40" width="7" height="50" rx="2"/>`,
  // The delta goes into the bucket, next to the reference.
  store: BUCKET_BACK + FILE(40, 14, 44, 58, 11) + LINES(47, 36, 28, 3, 9) +
    `<rect class="bf" x="102" y="6" width="7" height="44" rx="2"/><path class="b" d="M122 12v26M115 31l7 8l7-8"/>` + BUCKET_FRONT,
  // The file and the key, both on your side.
  key: FILE(22, 22, 44, 70, 12) + LINES(30, 48, 28, 4, 10) +
    `<circle class="b bt" cx="104" cy="60" r="13"/><circle class="bf" cx="104" cy="60" r="3.500"/><path class="b" d="M115 68l25 25M131 84l-8 8M139 92l-6 6"/>`,
  // A padlock: the file goes in, ciphertext comes out.
  lock: FILE(6, 44, 26, 42, 8) + ARROW(38, 65) +
    `<path class="b" d="M66 58v-14a14 14 0 0 1 28 0v14"/><rect class="b bt" x="56" y="58" width="48" height="40" rx="7"/><path class="b" d="M80 73v10"/>` +
    ARROW(110, 65) +
    `<g class="h"><rect x="124" y="46" width="9" height="9" rx="2"/><rect x="136" y="46" width="9" height="9" rx="2" opacity=".3"/><rect x="148" y="46" width="9" height="9" rx="2" opacity=".65"/><rect x="124" y="58" width="9" height="9" rx="2" opacity=".3"/><rect x="136" y="58" width="9" height="9" rx="2" opacity=".65"/><rect x="148" y="58" width="9" height="9" rx="2"/><rect x="124" y="70" width="9" height="9" rx="2" opacity=".65"/><rect x="136" y="70" width="9" height="9" rx="2"/><rect x="148" y="70" width="9" height="9" rx="2" opacity=".3"/></g>`,
  // The provider's bucket holds unreadable blocks only.
  'cipher-bucket': BUCKET_BACK +
    `<g class="h"><rect x="50" y="14" width="13" height="13" rx="2"/><rect x="66" y="14" width="13" height="13" rx="2" opacity=".3"/><rect x="82" y="14" width="13" height="13" rx="2" opacity=".65"/><rect x="98" y="14" width="13" height="13" rx="2" opacity=".3"/><rect x="50" y="30" width="13" height="13" rx="2" opacity=".65"/><rect x="66" y="30" width="13" height="13" rx="2"/><rect x="82" y="30" width="13" height="13" rx="2" opacity=".3"/><rect x="98" y="30" width="13" height="13" rx="2"/><rect x="50" y="46" width="13" height="13" rx="2" opacity=".3"/><rect x="66" y="46" width="13" height="13" rx="2" opacity=".65"/><rect x="82" y="46" width="13" height="13" rx="2"/><rect x="98" y="46" width="13" height="13" rx="2" opacity=".65"/></g>` +
    BUCKET_FRONT,
  // The proxy alone: the same chip as in the other scenes, drawn large.
  proxy: `<path class="k" d="M62 32v-11M80 32v-11M98 32v-11M62 109v-11M80 109v-11M98 109v-11"/>` +
    `<rect class="k c" x="48" y="32" width="64" height="66" rx="9"/><path class="b bt" d="M80 47l17 33h-34z"/>`,
};
