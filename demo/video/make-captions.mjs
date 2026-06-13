// Render captions.json → an ASS subtitle file (lower-third, branded styling).
// Times in captions.json are relative to the screen-capture portion; this
// script shifts them by the title-card duration so they line up in the final
// composed video. Usage: node make-captions.mjs <captions.json> <out.ass>

import { readFileSync, writeFileSync } from 'node:fs';

const [, , inPath, outPath] = process.argv;
const cfg = JSON.parse(readFileSync(inPath, 'utf8'));
const offset = Number(cfg.title_card_seconds || 0);

const toAss = (sec) => {
  const s = Math.max(0, sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = (s % 60).toFixed(2).padStart(5, '0');
  return `${h}:${String(m).padStart(2, '0')}:${ss}`;
};

// 1280x960 canvas. Lower-third caption band, San Francisco, brand green accent.
const header = `[Script Info]
ScriptType: v4.00+
PlayResX: 1280
PlayResY: 960
WrapStyle: 2
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Cap,SF Pro Display,34,&H00FFFFFF,&H00000000,&H96000000,1,0,3,0,0,2,90,90,70,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
`;

const lines = cfg.captions
  .map((c) => {
    const txt = c.text.replace(/\n/g, '\\N');
    return `Dialogue: 0,${toAss(c.start + offset)},${toAss(c.end + offset)},Cap,,0,0,0,,${txt}`;
  })
  .join('\n');

writeFileSync(outPath, header + lines + '\n');
console.log(`wrote ${outPath} (${cfg.captions.length} captions, +${offset}s offset)`);
