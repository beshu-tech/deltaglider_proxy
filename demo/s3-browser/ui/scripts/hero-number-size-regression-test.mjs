import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/heroNumberSize.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'heroNumberSize.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { heroFontPx } = await import(moduleUrl);

// Deliberate mirror of the module's per-character budgets (Manrope 800,
// tabular-nums, letter-spacing -0.045em). If the module's constants drift
// from these, the width-fit assertions below catch the regression.
const DIGIT_EM = 0.58;
const SEPARATOR_EM = 0.3;
const SUFFIX_EM = 0.42; // the '%' suffix share (rendered at 0.42em)
const emWidth = s =>
  [...s].reduce((w, ch) => w + (ch === ',' || ch === '.' ? SEPARATOR_EM : DIGIT_EM), SUFFIX_EM);

// Final hero strings across the legitimate range (1% … 100,000%) and the
// container widths the left grid column actually takes.
const CASES = ['1', '95', '268', '2,054', '10,500', '100,000'];
const WIDTHS = [240, 420, 700];

for (const width of WIDTHS) {
  let prevPx = Infinity;
  for (const s of CASES) {
    const px = heroFontPx(s.length, width);
    // ── bounds: always inside [40, 138] ─────────────────────────────────────
    assert.ok(px >= 40 && px <= 138, `bounds: "${s}" @ ${width}px -> ${px}px`);
    // ── monotonicity: more chars never yields a LARGER font ────────────────
    assert.ok(
      px <= prevPx + 1e-9,
      `monotone: "${s}" @ ${width}px -> ${px}px > previous ${prevPx}px`,
    );
    prevPx = px;
    // ── fit: estimated rendered width (chars × budget × px, incl. % share)
    //         never exceeds the container ───────────────────────────────────
    const estimated = emWidth(s) * px;
    assert.ok(
      estimated <= width + 0.01,
      `fit: "${s}" @ ${width}px -> estimated ${estimated.toFixed(2)}px overflows`,
    );
  }
}

// ── ceiling: short values still render huge ───────────────────────────────
assert.equal(heroFontPx('1'.length, 420), 138, '"1" at 420px hits the 138px ceiling');
assert.equal(heroFontPx('95'.length, 700), 138, '"95" at 700px hits the 138px ceiling');

// ── floor: absurdly long string in a narrow column clamps to 40, not below ─
assert.equal(heroFontPx(13, 240), 40, '13 chars at 240px clamps to the 40px floor');

// ── percent-lead branch shape ("95.1" — dot budgeted like a separator) ─────
assert.ok(heroFontPx('95.1'.length, 240) >= 40 && heroFontPx('95.1'.length, 240) <= 138);

// ── unmeasured / bogus widths fall back instead of exploding ───────────────
const fallback = heroFontPx('2,054'.length, 240);
assert.equal(heroFontPx('2,054'.length, 0), fallback, 'width 0 falls back to 240px column');
assert.equal(heroFontPx('2,054'.length, NaN), fallback, 'NaN width falls back to 240px column');

console.log('hero number size regression checks passed');
