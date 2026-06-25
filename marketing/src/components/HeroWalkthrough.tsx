// HeroWalkthrough.tsx — animated DeltaGlider product walkthrough for the hero.
//
// PORTRAIT (1080×1440, 3:4) so it fits the tall hero column naturally — no crop,
// no right-bleed. A 37s, 7-scene motion piece (Hook → delta compression →
// deltaspaces → drop-in S3 → IAM/ABAC → built-in UI → outro), self-contained:
// a tiny Stage/Sprite/Timeline engine + easing + 7 scenes. Pure React 19 + RAF.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from 'react';

// ── Easing ──────────────────────────────────────────────────────────────────
const Easing = {
  linear: (t: number) => t,
  easeInQuad: (t: number) => t * t,
  easeOutQuad: (t: number) => t * (2 - t),
  easeInCubic: (t: number) => t * t * t,
  easeOutCubic: (t: number) => --t * t * t + 1,
  easeInOutCubic: (t: number) =>
    t < 0.5 ? 4 * t * t * t : (t - 1) * (2 * t - 2) * (2 * t - 2) + 1,
  easeInOutSine: (t: number) => -(Math.cos(Math.PI * t) - 1) / 2,
  easeOutBack: (t: number) => {
    const c1 = 1.70158;
    const c3 = c1 + 1;
    return 1 + c3 * Math.pow(t - 1, 3) + c1 * Math.pow(t - 1, 2);
  },
};

const clamp = (v: number, min: number, max: number) => Math.max(min, Math.min(max, v));

// ── Timeline + Sprite contexts ───────────────────────────────────────────────
interface TimelineValue {
  time: number;
  duration: number;
  seek?: (t: number) => void;
}
const TimelineContext = createContext<TimelineValue>({ time: 0, duration: 37 });
const useTimeline = () => useContext(TimelineContext);

// Scene start times — shared by ProgressDots (jump targets) and the scenes.
const SCENE_MARKS = [0, 4.6, 13.4, 18.6, 23.2, 28.2, 33.4];
const SCENE_LABELS = ['Intro', 'Compression', 'Deltaspaces', 'Drop-in', 'Access', 'Browser', 'Outro'];

interface SpriteValue {
  localTime: number;
  progress: number;
  duration: number;
}
const SpriteContext = createContext<SpriteValue>({ localTime: 0, progress: 0, duration: 0 });
const useSprite = () => useContext(SpriteContext);

function Sprite({
  start = 0,
  end = Infinity,
  children,
}: {
  start?: number;
  end?: number;
  children: ReactNode;
}) {
  const { time } = useTimeline();
  const visible = time >= start && time <= end;
  if (!visible) return null;
  const duration = end - start;
  const localTime = Math.max(0, time - start);
  const progress = duration > 0 && isFinite(duration) ? clamp(localTime / duration, 0, 1) : 0;
  return (
    <SpriteContext.Provider value={{ localTime, progress, duration }}>
      {children}
    </SpriteContext.Provider>
  );
}

// ── Design tokens — light canvas, SITE brand accents (cyan #22d3ee + fuchsia
// #d946ef). The bright site hues are used for fills/bars; deeper derivatives
// carry text so they stay legible on the cream canvas. ───────────────────────
const C = {
  cream: '#EAF6F8', // faint cyan-tinted off-white (was warm teal)
  paper: '#FBFEFF',
  ink: '#0B1A2B', // site-navy-derived ink
  muted: '#5A6B7E',
  line: '#D2E4EA',
  lineSoft: '#E6F1F4',
  // accent = site cyan. `clay` (text/strokes) is a deepened cyan for contrast;
  // `clayBright` is the literal site #22d3ee for fills/bars/dots.
  clay: '#0E8BA8',
  clayBright: '#22d3ee',
  claySoft: '#9FE0EC',
  clayBg: '#D6F1F6',
  // secondary = site fuchsia, deepened for text on light.
  slate: '#B5179E',
  slateBg: '#F7E1F4',
  green: '#1FA98C',
  dark: '#0C1C2B', // terminal panel — site-navy family
  darkLine: '#1B3148',
};
const SERIF = "'Spectral', Georgia, serif";
const SANS = "'Hanken Grotesk', system-ui, sans-serif";
const MONO = "'JetBrains Mono', ui-monospace, monospace";

// Portrait stage. 3:4.
const W = 1080;
const H = 1440;
const PAD = 72; // common left/right margin

const ease = (t: number, fn: (t: number) => number = Easing.easeOutCubic) => fn(clamp(t, 0, 1));

// Scene fade/drift wrapper.
function Scene({
  start,
  end,
  fade = 0.5,
  drift = 0,
  scaleFrom = 1,
  scaleTo = 1,
  children,
}: {
  start: number;
  end: number;
  fade?: number;
  drift?: number;
  scaleFrom?: number;
  scaleTo?: number;
  children: ReactNode;
}) {
  const { time } = useTimeline();
  if (time < start - 0.001 || time > end + 0.001) return null;
  const dur = end - start;
  const lt = time - start;
  let opacity = 1;
  if (lt < fade) opacity = ease(lt / fade);
  else if (lt > dur - fade) opacity = ease((dur - lt) / fade);
  const p = clamp(lt / dur, 0, 1);
  const sc = scaleFrom + (scaleTo - scaleFrom) * Easing.easeInOutSine(p);
  const ty = drift * Easing.easeInOutSine(p);
  return (
    <div
      style={{
        position: 'absolute',
        inset: 0,
        opacity,
        transform: `scale(${sc}) translateY(${ty}px)`,
        transformOrigin: 'center',
      }}
    >
      {children}
    </div>
  );
}

// Top caption band — kicker + serif headline, centered across the portrait width.
function Caption({ kicker, text }: { kicker: string; text: ReactNode }) {
  const { localTime, duration } = useSprite();
  const inT = ease(localTime / 0.5);
  const outT = duration - localTime < 0.45 ? ease((duration - localTime) / 0.45) : 1;
  const o = Math.min(inT, outT);
  const ty = (1 - inT) * 14;
  return (
    <div
      style={{
        position: 'absolute',
        top: 120,
        left: PAD,
        right: PAD,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        gap: 22,
        opacity: o,
        transform: `translateY(${ty}px)`,
      }}
    >
      <div
        style={{
          fontFamily: MONO,
          fontSize: 22,
          letterSpacing: '0.24em',
          textTransform: 'uppercase',
          color: C.clay,
          fontWeight: 500,
          display: 'flex',
          alignItems: 'center',
          gap: 14,
        }}
      >
        <span style={{ width: 26, height: 1, background: C.clay, opacity: 0.5 }} />
        {kicker}
        <span style={{ width: 26, height: 1, background: C.clay, opacity: 0.5 }} />
      </div>
      <div
        style={{
          fontFamily: SERIF,
          fontSize: 60,
          fontWeight: 500,
          color: C.ink,
          letterSpacing: '-0.02em',
          textAlign: 'center',
          lineHeight: 1.12,
        }}
      >
        {text}
      </div>
    </div>
  );
}

function Logo({ size = 34, color = C.ink, mark = C.clayBright, gap = 16 }) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap }}>
      <svg width={size} height={size} viewBox="0 0 32 32" fill="none">
        <path d="M16 4 L28 26 L16 20 L4 26 Z" fill={mark} />
      </svg>
      <span
        style={{
          fontFamily: SERIF,
          fontWeight: 600,
          fontSize: size * 0.92,
          color,
          letterSpacing: '-0.01em',
        }}
      >
        DeltaGlider
      </span>
    </div>
  );
}

function Card({
  x,
  y,
  w,
  h,
  children,
  style = {},
}: {
  x: number;
  y: number;
  w: number;
  h: number;
  children?: ReactNode;
  style?: CSSProperties;
}) {
  return (
    <div
      style={{
        position: 'absolute',
        left: x,
        top: y,
        width: w,
        height: h,
        background: C.paper,
        border: `1px solid ${C.line}`,
        borderRadius: 20,
        boxShadow: '0 24px 60px -30px rgba(14,42,50,0.30)',
        ...style,
      }}
    >
      {children}
    </div>
  );
}

// ── SCENE 1 — HOOK (0–4.6) ────────────────────────────────────────────────────
function SceneHook() {
  const { time } = useTimeline();
  const lt = time;
  const line1 = ease((lt - 0.5) / 0.7, Easing.easeOutCubic);
  const big = ease((lt - 1.0) / 0.8, Easing.easeOutBack);
  const line3 = ease((lt - 1.9) / 0.7);
  const logoO = ease((lt - 0.0) / 0.6);
  return (
    <Scene start={0} end={4.6} fade={0.45} scaleFrom={1.0} scaleTo={1.035}>
      <div style={{ position: 'absolute', top: 96, left: 0, right: 0, display: 'flex', justifyContent: 'center', opacity: logoO }}>
        <Logo size={40} />
      </div>
      <div
        style={{
          position: 'absolute',
          inset: 0,
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 4,
          padding: `0 ${PAD}px`,
          textAlign: 'center',
        }}
      >
        <div
          style={{
            fontFamily: SANS,
            fontSize: 38,
            color: C.muted,
            fontWeight: 500,
            opacity: line1,
            transform: `translateY(${(1 - line1) * 12}px)`,
          }}
        >
          An 82&nbsp;MB release upload.
        </div>
        <div
          style={{
            fontFamily: SERIF,
            fontSize: 132,
            fontWeight: 600,
            color: C.ink,
            letterSpacing: '-0.03em',
            opacity: big,
            lineHeight: 1.04,
            transform: `translateY(${(1 - big) * 22}px)`,
            marginTop: 18,
          }}
        >
          Stored as
          <br />
          <span style={{ color: C.clay }}>1.4&nbsp;MB.</span>
        </div>
        <div
          style={{
            fontFamily: SANS,
            fontSize: 32,
            color: C.muted,
            fontWeight: 450,
            opacity: line3,
            transform: `translateY(${(1 - line3) * 12}px)`,
            marginTop: 56,
            lineHeight: 1.4,
          }}
        >
          Reconstructed byte-identical.
          <br />
          Clients see standard S3.
        </div>
      </div>
    </Scene>
  );
}

function Stat({
  label,
  value,
  accent,
  show,
  t,
}: {
  label: string;
  value: string;
  accent: string;
  show: boolean;
  t: number;
}) {
  const o = show ? ease(t / 0.5) : 0;
  return (
    <div style={{ opacity: o, transform: `translateY(${(1 - o) * 10}px)` }}>
      <div
        style={{
          fontFamily: MONO,
          fontSize: 17,
          letterSpacing: '0.14em',
          textTransform: 'uppercase',
          color: C.muted,
          marginBottom: 10,
        }}
      >
        {label}
      </div>
      <div
        style={{
          fontFamily: SERIF,
          fontSize: 64,
          fontWeight: 700,
          color: accent,
          lineHeight: 1,
          letterSpacing: '-0.02em',
          fontVariantNumeric: 'tabular-nums',
        }}
      >
        {value}
      </div>
    </div>
  );
}

// ── SCENE 2 — DELTA COMPRESSION (4.6–13.4) ────────────────────────────────────
function SceneCompression() {
  const S = 4.6;
  const E = 13.4;
  const { time } = useTimeline();
  const lt = time - S;

  const cmd = '$ aws s3 cp \\\n    releases/v2.zip \\\n    s3://builds/releases/';
  const typeT = clamp((lt - 0.8) / 1.4, 0, 1);
  const shown = cmd.slice(0, Math.round(typeT * cmd.length));
  const uploaded = lt > 2.4;

  const collapseStart = 3.4;
  const collapseDur = 1.8;
  const cp = ease((lt - collapseStart) / collapseDur, Easing.easeInOutCubic);
  const fullW = W - PAD * 2;
  const storedW = Math.max(14, fullW * 0.017);
  const barW = fullW - (fullW - storedW) * cp;

  const sizeNow = 82.0 - (82.0 - 1.4) * cp;
  const pctNow = cp * 98.3;
  const showStored = lt > collapseStart + 0.2;
  const showGet = lt > 6.4;

  return (
    <Scene start={S} end={E} fade={0.5}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Delta compression"
          text={
            <>
              Transparent <span style={{ color: C.clay }}>xdelta3</span>
              <br />
              diff vs a baseline.
            </>
          }
        />
      </Sprite>

      {/* Terminal (multi-line in portrait) */}
      <Card
        x={PAD}
        y={400}
        w={fullW}
        h={250}
        style={{ background: C.dark, border: `1px solid ${C.darkLine}`, borderRadius: 16 }}
      >
        <div style={{ display: 'flex', gap: 10, padding: '22px 0 0 26px' }}>
          {['#E5685A', '#E6B14C', '#62B25A'].map((c, i) => (
            <span
              key={i}
              style={{ width: 15, height: 15, borderRadius: 8, background: c, opacity: 0.92 }}
            />
          ))}
        </div>
        <div
          style={{
            padding: '20px 30px',
            fontFamily: MONO,
            fontSize: 28,
            color: '#DCEEF0',
            letterSpacing: '0.01em',
            whiteSpace: 'pre-wrap',
            lineHeight: 1.45,
          }}
        >
          {shown}
          {typeT < 1 && <span style={{ opacity: Math.floor(lt * 2) % 2 ? 1 : 0.1 }}>▋</span>}
          {uploaded && (
            <span style={{ color: '#5FD0C4', opacity: ease((lt - 2.5) / 0.4) }}>{'  ✓ 200 OK'}</span>
          )}
        </div>
      </Card>

      {/* Bar viz */}
      <div style={{ position: 'absolute', left: PAD, top: 760, width: fullW }}>
        <div
          style={{
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'flex-end',
            marginBottom: 18,
          }}
        >
          <div style={{ fontFamily: MONO, fontSize: 24, color: C.muted, letterSpacing: '0.02em' }}>
            releases/v2.zip
          </div>
          <div
            style={{
              fontFamily: SERIF,
              fontSize: 36,
              color: C.ink,
              fontWeight: 600,
              fontVariantNumeric: 'tabular-nums',
            }}
          >
            {sizeNow.toFixed(sizeNow < 10 ? 1 : 0)} MB
          </div>
        </div>

        <div style={{ position: 'relative', height: 84 }}>
          <div
            style={{
              position: 'absolute',
              left: 0,
              top: 0,
              width: fullW,
              height: 84,
              border: `2px dashed ${C.claySoft}`,
              borderRadius: 14,
              opacity: cp * 0.9,
            }}
          />
          <div
            style={{
              position: 'absolute',
              left: 0,
              top: 0,
              width: barW,
              height: 84,
              background: `linear-gradient(180deg, ${C.clayBright}, ${C.clay})`,
              borderRadius: 14,
              boxShadow: '0 10px 28px -12px rgba(34,211,238,0.55)',
            }}
          />
        </div>
        <div
          style={{
            marginTop: 14,
            height: 28,
            opacity: ease((lt - collapseStart - 0.2) / 0.5),
            fontFamily: SANS,
            fontSize: 23,
            color: C.clay,
            fontWeight: 600,
          }}
        >
          ↑ only the changed bytes are stored
        </div>

        {/* stats — side by side, fits portrait width */}
        <div style={{ display: 'flex', gap: 80, marginTop: 40 }}>
          <Stat
            label="On backend"
            value="1.4 MB"
            accent={C.clay}
            show={showStored}
            t={lt - collapseStart}
          />
          <Stat
            label="Reduction"
            value={`−${pctNow.toFixed(pctNow > 9 ? 0 : 1)}%`}
            accent={C.ink}
            show={showStored}
            t={lt - collapseStart - 0.15}
          />
        </div>

        {/* GET line */}
        <div
          style={{
            marginTop: 44,
            opacity: ease((lt - 6.4) / 0.6),
            transform: `translateY(${(1 - ease((lt - 6.4) / 0.6)) * 12}px)`,
            display: showGet ? 'flex' : 'none',
            alignItems: 'center',
            gap: 14,
            fontFamily: SANS,
            fontSize: 25,
            color: C.muted,
            fontWeight: 500,
          }}
        >
          <span
            style={{
              fontFamily: MONO,
              fontSize: 20,
              color: C.green,
              background: '#DEF0ED',
              padding: '8px 14px',
              borderRadius: 9,
              fontWeight: 600,
            }}
          >
            GET
          </span>
          rebuilt byte-identical · SHA-256 verified
        </div>
      </div>
    </Scene>
  );
}

// ── SCENE 3 — DELTASPACES (13.4–18.6) ─────────────────────────────────────────
function SceneDeltaspace() {
  const S = 13.4;
  const E = 18.6;
  const { time } = useTimeline();
  const lt = time - S;
  const rows = [
    { n: 'v1.zip', tag: 'baseline', sub: 'seeds the reference', size: '82 MB', kind: 'base' },
    { n: 'v2.zip', tag: 'delta', sub: 'xdelta3 vs baseline', size: '1.4 MB', kind: 'delta' },
    { n: 'v3.zip', tag: 'delta', sub: 'xdelta3 vs baseline', size: '0.9 MB', kind: 'delta' },
  ];
  const cardW = W - PAD * 2;
  return (
    <Scene start={S} end={E} fade={0.5} drift={-14}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Deltaspaces"
          text={
            <>
              One prefix,
              <br />
              <span style={{ color: C.clay }}>one baseline.</span>
            </>
          }
        />
      </Sprite>
      <Card x={PAD} y={480} w={cardW} h={620}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 16,
            padding: '34px 40px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <svg width="30" height="30" viewBox="0 0 24 24" fill="none">
            <path d="M3 6h6l2 2h10v11H3z" stroke={C.clay} strokeWidth="1.7" />
          </svg>
          <span style={{ fontFamily: MONO, fontSize: 30, color: C.ink, fontWeight: 600 }}>
            deltaspace
          </span>
          <span style={{ fontFamily: MONO, fontSize: 30, color: C.clay, fontWeight: 600 }}>
            releases/
          </span>
        </div>
        {rows.map((r, i) => {
          const o = ease((lt - 0.4 - i * 0.3) / 0.5);
          const isBase = r.kind === 'base';
          return (
            <div
              key={i}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 24,
                padding: '36px 40px',
                borderBottom: i < rows.length - 1 ? `1px solid ${C.lineSoft}` : 'none',
                opacity: o,
                transform: `translateX(${(1 - o) * 22}px)`,
                background: isBase ? C.clayBg + '88' : 'transparent',
              }}
            >
              <span
                style={{ fontFamily: MONO, fontSize: 30, color: C.ink, fontWeight: 600, width: 150 }}
              >
                {r.n}
              </span>
              <span
                style={{
                  fontFamily: MONO,
                  fontSize: 18,
                  fontWeight: 600,
                  letterSpacing: '0.06em',
                  textTransform: 'uppercase',
                  color: isBase ? C.slate : C.clay,
                  background: isBase ? C.slateBg : C.clayBg,
                  padding: '8px 16px',
                  borderRadius: 9,
                }}
              >
                {r.tag}
              </span>
              <span style={{ flex: 1 }} />
              <span
                style={{
                  fontFamily: SERIF,
                  fontSize: 38,
                  fontWeight: 700,
                  color: isBase ? C.ink : C.clay,
                  fontVariantNumeric: 'tabular-nums',
                }}
              >
                {r.size}
              </span>
            </div>
          );
        })}
      </Card>
      <div
        style={{
          position: 'absolute',
          top: 1150,
          left: PAD,
          right: PAD,
          textAlign: 'center',
          fontFamily: MONO,
          fontSize: 22,
          color: C.muted,
          opacity: ease((lt - 1.4) / 0.6),
          lineHeight: 1.5,
        }}
      >
        the first upload seeds the baseline —
        <br />
        later versions are stored as diffs
      </div>
    </Scene>
  );
}

// ── SCENE 4 — DROP-IN S3 (18.6–23.2) — VERTICAL flow in portrait ──────────────
function SceneDropIn() {
  const S = 18.6;
  const E = 23.2;
  const { time } = useTimeline();
  const lt = time - S;
  const flow = (lt % 1.6) / 1.6;
  const nodeW = 620;
  const nodeX = (W - nodeW) / 2; // centred
  const nodeH = 180;
  const gapY = 84;
  const startY = 470;
  const nodes = [
    { title: 'S3 clients', lines: ['aws-cli · SDKs · presigned URLs'], brand: false },
    { title: 'DeltaGlider', lines: ['delta · reconstruct · SigV4 · cache'], brand: true },
    { title: 'Backend', lines: ['Filesystem · S3 / MinIO'], brand: false },
  ];
  return (
    <Scene start={S} end={E} fade={0.5} drift={-10}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="S3-compatible"
          text={
            <>
              Drop-in.
              <br />
              <span style={{ color: C.clay }}>Clients never know.</span>
            </>
          }
        />
      </Sprite>
      {/* vertical connectors with flowing dot */}
      <svg width={W} height={H} style={{ position: 'absolute', inset: 0, pointerEvents: 'none' }}>
        {[0, 1].map((i) => {
          const x = W / 2;
          const yTop = startY + nodeH + (nodeH + gapY) * i;
          const yBot = yTop + gapY;
          const o = ease((lt - 0.5 - i * 0.2) / 0.5);
          const dy = yTop + (yBot - yTop) * ((flow + i * 0.5) % 1);
          return (
            <g key={i}>
              <line
                x1={x}
                y1={yTop}
                x2={x}
                y2={yBot}
                stroke={C.clay}
                strokeWidth="2.5"
                strokeDasharray="2 9"
                strokeLinecap="round"
                opacity={o * 0.6}
              />
              {lt > 0.8 ? <circle cx={x} cy={dy} r="7" fill={C.clayBright} opacity={o} /> : null}
            </g>
          );
        })}
      </svg>
      {nodes.map((n, i) => {
        const o = ease((lt - 0.3 - i * 0.22) / 0.5);
        const y = startY + (nodeH + gapY) * i;
        return (
          <Card
            key={i}
            x={nodeX}
            y={y}
            w={nodeW}
            h={nodeH}
            style={{
              opacity: o,
              transform: `translateY(${(1 - o) * 18}px)`,
              display: 'flex',
              flexDirection: 'column',
              alignItems: 'center',
              justifyContent: 'center',
              gap: 14,
              border: n.brand ? `2px solid ${C.clay}` : `1px solid ${C.line}`,
            }}
          >
            {n.brand ? (
              <Logo size={34} />
            ) : (
              <div style={{ fontFamily: SANS, fontSize: 34, fontWeight: 700, color: C.ink }}>
                {n.title}
              </div>
            )}
            {n.lines.map((l, j) => (
              <div key={j} style={{ fontFamily: MONO, fontSize: 22, color: C.muted }}>
                {l}
              </div>
            ))}
          </Card>
        );
      })}
      {/* op chips */}
      <div
        style={{
          position: 'absolute',
          top: startY + (nodeH + gapY) * 3 + 20,
          left: PAD,
          right: PAD,
          display: 'flex',
          justifyContent: 'center',
          gap: 14,
          flexWrap: 'wrap',
          opacity: ease((lt - 1.2) / 0.6),
        }}
      >
        {['PutObject', 'GetObject', 'Multipart', 'ListObjectsV2', 'Range', 'CopyObject'].map(
          (op, i) => (
            <span
              key={i}
              style={{
                fontFamily: MONO,
                fontSize: 22,
                color: C.ink,
                background: C.paper,
                border: `1px solid ${C.line}`,
                padding: '12px 22px',
                borderRadius: 11,
              }}
            >
              {op}
            </span>
          ),
        )}
      </div>
    </Scene>
  );
}

// ── SCENE 5 — IAM / ABAC (23.2–28.2) ──────────────────────────────────────────
function SceneIAM() {
  const S = 23.2;
  const E = 28.2;
  const { time } = useTimeline();
  const lt = time - S;
  const users = [
    { name: 'ci-pipeline', role: 'Service account', tag: 'write: builds/*' },
    { name: 'mara.k', role: 'Release engineer', tag: 'read · write: releases/*' },
    { name: 'auditor', role: 'Read-only', tag: 'read: *' },
  ];
  const cardW = W - PAD * 2;
  return (
    <Scene start={S} end={E} fade={0.5} drift={-12}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Access control"
          text={
            <>
              Per-user IAM,
              <br />
              <span style={{ color: C.clay }}>fine-grained ABAC.</span>
            </>
          }
        />
      </Sprite>
      <Card x={PAD} y={470} w={cardW} h={680}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            padding: '30px 38px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <div style={{ fontFamily: SANS, fontSize: 28, fontWeight: 600, color: C.ink }}>
            Access policy
          </div>
          <div style={{ display: 'flex', gap: 10, opacity: ease((lt - 0.3) / 0.5) }}>
            {['SigV4', 'ABAC'].map((b, i) => (
              <span
                key={i}
                style={{
                  fontFamily: MONO,
                  fontSize: 18,
                  color: C.slate,
                  background: C.slateBg,
                  padding: '8px 14px',
                  borderRadius: 9,
                  fontWeight: 600,
                }}
              >
                {b}
              </span>
            ))}
          </div>
        </div>
        {users.map((u, i) => {
          const o = ease((lt - 0.4 - i * 0.28) / 0.5);
          return (
            <div
              key={i}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 22,
                padding: '30px 38px',
                borderBottom: `1px solid ${C.lineSoft}`,
                opacity: o,
                transform: `translateX(${(1 - o) * 22}px)`,
              }}
            >
              <div
                style={{
                  width: 62,
                  height: 62,
                  borderRadius: 31,
                  flexShrink: 0,
                  background: i === 0 ? C.clayBg : C.slateBg,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  fontFamily: SANS,
                  fontWeight: 700,
                  fontSize: 26,
                  color: i === 0 ? C.clay : C.slate,
                }}
              >
                {u.name[0].toUpperCase()}
              </div>
              <div style={{ flex: 1 }}>
                <div style={{ fontFamily: MONO, fontSize: 26, color: C.ink, fontWeight: 600 }}>
                  {u.name}
                </div>
                <div style={{ fontFamily: MONO, fontSize: 20, color: C.clay, marginTop: 6 }}>
                  {u.tag}
                </div>
              </div>
            </div>
          );
        })}
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 14,
            padding: '28px 38px',
            fontFamily: SANS,
            fontSize: 22,
            color: C.muted,
            opacity: ease((lt - 1.5) / 0.6),
          }}
        >
          <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
            <rect x="5" y="11" width="14" height="9" rx="2" fill={C.green} />
            <path d="M8 11V8a4 4 0 0 1 8 0v3" stroke={C.green} strokeWidth="2" />
          </svg>
          Encrypted SQLCipher database
        </div>
      </Card>
    </Scene>
  );
}

// ── SCENE 6 — BUILT-IN UI / BROWSER (28.2–33.4) ───────────────────────────────
function SceneBrowser() {
  const S = 28.2;
  const E = 33.4;
  const { time } = useTimeline();
  const lt = time - S;
  // Faithful to the REAL object browser: a file list + the inspector drawer
  // with the savings panel (Original→Stored bars, Delta badge) and dg-* metadata.
  const files = ['app-v1.zip', 'app-v2.zip', 'app-v3.zip', 'app-v4.zip', 'app-v5.zip'];
  const selected = 3; // app-v4.zip — the one open in the inspector
  const cardW = W - PAD * 2;
  const barFill = ease((lt - 1.2) / 0.9, Easing.easeInOutCubic);
  const meta = [
    { k: 'dg-delta-cmd', v: 'xdelta3 -e -9 -s reference.bin' },
    { k: 'dg-delta-size', v: '1389 bytes' },
  ];
  return (
    <Scene start={S} end={E} fade={0.5} scaleFrom={1.02} scaleTo={1.0}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Built-in UI"
          text={
            <>
              Browse &amp; inspect,
              <br />
              <span style={{ color: C.clay }}>served at /_/</span>
            </>
          }
        />
      </Sprite>

      {/* File list */}
      <Card x={PAD} y={460} w={cardW} h={250} style={{ overflow: 'hidden', padding: '8px 0' }}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 12,
            padding: '14px 30px 18px',
            fontFamily: MONO,
            fontSize: 20,
            color: C.muted,
          }}
        >
          <span style={{ color: C.ink, fontWeight: 600 }}>demo-bucket</span>
          <span style={{ color: C.line }}>›</span>
          <span style={{ color: C.ink, fontWeight: 600 }}>demo-releases</span>
        </div>
        {files.map((f, i) => {
          const o = ease((lt - 0.3 - i * 0.1) / 0.4);
          const sel = i === selected;
          return (
            <div
              key={i}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 14,
                padding: '0 30px',
                height: 40,
                opacity: o,
                background: sel ? C.clayBg : 'transparent',
                borderLeft: sel ? `4px solid ${C.clayBright}` : '4px solid transparent',
                fontFamily: MONO,
                fontSize: 22,
                color: C.ink,
              }}
            >
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none">
                <path d="M6 3h8l4 4v14H6z" stroke={C.muted} strokeWidth="1.6" />
                <path d="M14 3v4h4" stroke={C.muted} strokeWidth="1.6" />
              </svg>
              {f}
            </div>
          );
        })}
      </Card>

      {/* Inspector drawer */}
      <Card
        x={PAD}
        y={726}
        w={cardW}
        h={620}
        style={{ overflow: 'hidden', opacity: ease((lt - 0.7) / 0.5) }}
      >
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 14,
            padding: '26px 32px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <svg width="26" height="26" viewBox="0 0 24 24" fill="none">
            <path d="M6 3h8l4 4v14H6z" stroke={C.clay} strokeWidth="1.6" />
            <path d="M14 3v4h4" stroke={C.clay} strokeWidth="1.6" />
          </svg>
          <span style={{ fontFamily: MONO, fontSize: 28, color: C.ink, fontWeight: 600 }}>
            app-v4.zip
          </span>
        </div>

        {/* Savings panel */}
        <div style={{ padding: '28px 32px' }}>
          <div
            style={{
              fontFamily: MONO,
              fontSize: 16,
              letterSpacing: '0.12em',
              textTransform: 'uppercase',
              color: C.muted,
              textAlign: 'center',
            }}
          >
            Savings
          </div>
          <div
            style={{
              fontFamily: SERIF,
              fontSize: 88,
              fontWeight: 700,
              color: C.green,
              textAlign: 'center',
              lineHeight: 1.05,
              letterSpacing: '-0.02em',
            }}
          >
            97.2%
          </div>
          <div
            style={{
              fontFamily: SANS,
              fontSize: 20,
              color: C.muted,
              textAlign: 'center',
              marginBottom: 24,
            }}
          >
            47.5 KB saved
          </div>

          {[
            { label: 'Original', val: '48.8 KB', frac: 1 },
            { label: 'Stored', val: '1.4 KB', frac: 0.028 },
          ].map((b, i) => (
            <div key={i} style={{ marginBottom: 16 }}>
              <div
                style={{
                  display: 'flex',
                  justifyContent: 'space-between',
                  fontFamily: MONO,
                  fontSize: 20,
                  color: i === 0 ? C.muted : C.clay,
                  fontWeight: i === 0 ? 400 : 600,
                  marginBottom: 8,
                }}
              >
                <span>{b.label}</span>
                <span>{b.val}</span>
              </div>
              <div style={{ height: 10, borderRadius: 5, background: C.lineSoft }}>
                <div
                  style={{
                    width: `${b.frac * 100 * (i === 0 ? 1 : barFill) + (i === 0 ? 0 : 0)}%`,
                    height: 10,
                    borderRadius: 5,
                    background: i === 0 ? C.claySoft : C.clayBright,
                  }}
                />
              </div>
            </div>
          ))}

          <div style={{ textAlign: 'center', marginTop: 18 }}>
            <span
              style={{
                fontFamily: MONO,
                fontSize: 18,
                fontWeight: 600,
                color: C.slate,
                background: C.slateBg,
                padding: '8px 18px',
                borderRadius: 9,
                letterSpacing: '0.04em',
              }}
            >
              Delta
            </span>
          </div>
        </div>

        {/* Custom metadata */}
        <div style={{ padding: '6px 32px' }}>
          {meta.map((m, i) => {
            const o = ease((lt - 1.8 - i * 0.18) / 0.5);
            return (
              <div
                key={i}
                style={{
                  background: C.cream,
                  borderRadius: 10,
                  padding: '14px 18px',
                  marginBottom: 10,
                  opacity: o,
                  transform: `translateY(${(1 - o) * 8}px)`,
                }}
              >
                <div style={{ fontFamily: MONO, fontSize: 16, color: C.muted }}>{m.k}</div>
                <div style={{ fontFamily: MONO, fontSize: 20, color: C.ink, marginTop: 4 }}>
                  {m.v}
                </div>
              </div>
            );
          })}
        </div>
      </Card>
    </Scene>
  );
}

// ── SCENE 7 — OUTRO (33.4–37) ─────────────────────────────────────────────────
function SceneOutro() {
  const S = 33.4;
  const E = 37.0;
  const { time } = useTimeline();
  const lt = time - S;
  const logoO = ease((lt - 0.2) / 0.7, Easing.easeOutBack);
  const tagO = ease((lt - 0.9) / 0.7);
  return (
    <Scene start={S} end={E} fade={0.55}>
      <div
        style={{
          position: 'absolute',
          inset: 0,
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 40,
          padding: `0 ${PAD}px`,
          textAlign: 'center',
        }}
      >
        <div style={{ opacity: logoO, transform: `translateY(${(1 - logoO) * 14}px)` }}>
          <Logo size={72} />
        </div>
        <div
          style={{
            fontFamily: SERIF,
            fontSize: 50,
            fontWeight: 500,
            color: C.ink,
            opacity: tagO,
            transform: `translateY(${(1 - tagO) * 12}px)`,
            lineHeight: 1.3,
            letterSpacing: '-0.015em',
          }}
        >
          Delta-compress versioned binaries. Storage drops{' '}
          <span style={{ color: C.clay }}>60–95%.</span>
        </div>
        <div
          style={{
            fontFamily: MONO,
            fontSize: 24,
            color: C.muted,
            letterSpacing: '0.04em',
            opacity: ease((lt - 1.6) / 0.6),
            marginTop: 8,
          }}
        >
          docker run beshultd/deltaglider_proxy
        </div>
      </div>
    </Scene>
  );
}

function Vignette() {
  return (
    <div
      style={{
        position: 'absolute',
        inset: 0,
        pointerEvents: 'none',
        background:
          'radial-gradient(120% 100% at 50% 35%, transparent 55%, rgba(12,40,48,0.06) 100%)',
      }}
    />
  );
}

function ProgressDots() {
  const { time, seek } = useTimeline();
  let active = 0;
  SCENE_MARKS.forEach((m, i) => {
    if (time >= m) active = i;
  });
  return (
    <div
      style={{
        position: 'absolute',
        bottom: 56,
        left: 0,
        right: 0,
        display: 'flex',
        justifyContent: 'center',
        gap: 18,
        pointerEvents: 'auto', // plate is pointer-events:none — opt back in
      }}
    >
      {SCENE_MARKS.map((m, i) => (
        <button
          key={i}
          type="button"
          onClick={() => seek?.(m + 0.6)}
          aria-label={`Go to ${SCENE_LABELS[i]}`}
          title={SCENE_LABELS[i]}
          style={{
            width: i === active ? 44 : 26,
            height: 26,
            padding: 0,
            border: 'none',
            background: 'transparent',
            cursor: 'pointer',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
          }}
        >
          {/* the visible pill — bigger transparent hit-area around it */}
          <span
            style={{
              width: i === active ? 40 : 11,
              height: 11,
              borderRadius: 6,
              background: i === active ? C.clayBright : C.line,
              transition: 'width 0.35s, background 0.35s',
            }}
          />
        </button>
      ))}
    </div>
  );
}

// ── Hero walkthrough: contain-fit portrait canvas + autoplay loop + play/pause ─
const DURATION = 37;

export default function HeroWalkthrough() {
  const wrapRef = useRef<HTMLDivElement>(null);
  const rafRef = useRef<number | null>(null);
  const lastTsRef = useRef<number | null>(null);
  const [time, setTime] = useState(0);
  const [playing, setPlaying] = useState(true);
  const [frame, setFrame] = useState({ w: 0, h: 0, scale: 1 });

  // CONTAIN-fit the portrait W×H canvas inside the frame; the frame maximizes to
  // the column (capped to the W/H aspect) so the whole composition is visible.
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const measure = () => {
      const cs = getComputedStyle(el);
      const availW = el.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
      const availH = el.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
      if (availW <= 1 || availH <= 1) return;
      // Fit the W:H (portrait) box inside the available area.
      const fh = Math.min(availH, availW * (H / W));
      const fw = fh * (W / H);
      setFrame({ w: fw, h: fh, scale: fw / W });
    };
    measure();
    const raf = requestAnimationFrame(measure);
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  // Autoplay RAF loop (loops at DURATION). Paused state stops it.
  useEffect(() => {
    if (!playing) {
      lastTsRef.current = null;
      return;
    }
    const step = (ts: number) => {
      if (lastTsRef.current == null) lastTsRef.current = ts;
      const dt = (ts - lastTsRef.current) / 1000;
      lastTsRef.current = ts;
      setTime((t) => (t + dt) % DURATION);
      rafRef.current = requestAnimationFrame(step);
    };
    rafRef.current = requestAnimationFrame(step);
    return () => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
      lastTsRef.current = null;
    };
  }, [playing]);

  // Jump to a scene; reset the RAF delta so the loop continues smoothly from there.
  const seek = useCallback((t: number) => {
    lastTsRef.current = null;
    setTime(clamp(t, 0, DURATION));
  }, []);
  const ctx = useMemo(() => ({ time, duration: DURATION, seek }), [time, seek]);

  return (
    <div ref={wrapRef} className="hero-walkthrough">
      <div className="hero-walkthrough__glow" aria-hidden="true" />

      <div
        className="hero-walkthrough__frame"
        style={{ width: frame.w || undefined, height: frame.h || undefined }}
      >
        <div
          className="hero-walkthrough__canvas"
          style={{ width: W, height: H, zoom: frame.scale, background: C.cream }}
        >
          <TimelineContext.Provider value={ctx}>
            <Vignette />
            <SceneHook />
            <SceneCompression />
            <SceneDeltaspace />
            <SceneDropIn />
            <SceneIAM />
            <SceneBrowser />
            <SceneOutro />
            <ProgressDots />
          </TimelineContext.Provider>
        </div>

        <button
          type="button"
          className="hero-walkthrough__toggle"
          onClick={() => setPlaying((p) => !p)}
          aria-label={playing ? 'Pause walkthrough' : 'Play walkthrough'}
          title={playing ? 'Pause' : 'Play'}
        >
          {playing ? (
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">
              <rect x="3" y="2" width="3" height="10" fill="currentColor" />
              <rect x="8" y="2" width="3" height="10" fill="currentColor" />
            </svg>
          ) : (
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">
              <path d="M3 2l9 5-9 5V2z" fill="currentColor" />
            </svg>
          )}
        </button>
      </div>
    </div>
  );
}
