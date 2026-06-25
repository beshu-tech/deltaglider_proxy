// HeroWalkthrough.tsx — animated DeltaGlider product walkthrough for the hero.
//
// Ported from the Claude Design "Deltaglider proxy product walkthrough" (Video.jsx)
// and adapted for the hero plate: autoplay-loop + minimal play/pause only (no
// scrubber/timecode/keyboard chrome), cover-scaled into the plate, returns null
// under prefers-reduced-motion so the screenshot slideshow fallback takes over.
//
// Self-contained: a tiny Stage/Sprite/Timeline engine + easing helpers + 7 scenes.
// Pure React 19 + requestAnimationFrame, no animation lib (matches the site).

import {
  createContext,
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
}
const TimelineContext = createContext<TimelineValue>({ time: 0, duration: 37 });
const useTimeline = () => useContext(TimelineContext);

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

// ── Design tokens (teal / blue) ───────────────────────────────────────────────
const C = {
  cream: '#ECF3F4',
  paper: '#FBFDFD',
  ink: '#0E2A32',
  muted: '#5C737A',
  line: '#D6E3E5',
  lineSoft: '#E6EFF0',
  clay: '#0E9AA4',
  claySoft: '#A9DCDF',
  clayBg: '#DBEFF0',
  slate: '#2B6CB0',
  slateBg: '#E1ECF6',
  green: '#2A9D8F',
  dark: '#0C242B',
  darkLine: '#1A3B45',
};
const SERIF = "'Spectral', Georgia, serif";
const SANS = "'Hanken Grotesk', system-ui, sans-serif";
const MONO = "'JetBrains Mono', ui-monospace, monospace";

const W = 1920;
const H = 1080;

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
        top: 96,
        left: 0,
        right: 0,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        gap: 18,
        opacity: o,
        transform: `translateY(${ty}px)`,
      }}
    >
      <div
        style={{
          fontFamily: MONO,
          fontSize: 19,
          letterSpacing: '0.22em',
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
          fontSize: 56,
          fontWeight: 500,
          color: C.ink,
          letterSpacing: '-0.02em',
          textAlign: 'center',
          maxWidth: 1240,
          lineHeight: 1.08,
        }}
      >
        {text}
      </div>
    </div>
  );
}

function Logo({ size = 34, color = C.ink, mark = C.clay, gap = 16 }) {
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
        borderRadius: 18,
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
      <div style={{ position: 'absolute', top: 80, left: 96, opacity: logoO }}>
        <Logo size={32} />
      </div>
      <div
        style={{
          position: 'absolute',
          inset: 0,
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 8,
        }}
      >
        <div
          style={{
            fontFamily: SANS,
            fontSize: 30,
            color: C.muted,
            fontWeight: 500,
            opacity: line1,
            transform: `translateY(${(1 - line1) * 12}px)`,
            letterSpacing: '0.01em',
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
            letterSpacing: '-0.035em',
            opacity: big,
            whiteSpace: 'nowrap',
            transform: `translateY(${(1 - big) * 22}px)`,
            lineHeight: 1.0,
            marginTop: 6,
          }}
        >
          Stored as <span style={{ color: C.clay }}>1.4&nbsp;MB.</span>
        </div>
        <div
          style={{
            fontFamily: SANS,
            fontSize: 28,
            color: C.muted,
            fontWeight: 450,
            opacity: line3,
            transform: `translateY(${(1 - line3) * 12}px)`,
            marginTop: 46,
            letterSpacing: '0.005em',
          }}
        >
          Reconstructed byte-identical. Clients see standard S3.
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
          fontSize: 15,
          letterSpacing: '0.14em',
          textTransform: 'uppercase',
          color: C.muted,
          marginBottom: 8,
        }}
      >
        {label}
      </div>
      <div
        style={{
          fontFamily: SERIF,
          fontSize: 52,
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

  const cmd = '$ aws s3 cp releases/v2.zip s3://builds/releases/';
  const typeT = clamp((lt - 0.8) / 1.4, 0, 1);
  const shown = cmd.slice(0, Math.round(typeT * cmd.length));
  const uploaded = lt > 2.4;

  const collapseStart = 3.4;
  const collapseDur = 1.8;
  const cp = ease((lt - collapseStart) / collapseDur, Easing.easeInOutCubic);
  const fullW = 1180;
  const storedW = Math.max(12, fullW * 0.017);
  const barW = fullW - (fullW - storedW) * cp;

  const sizeNow = 82.0 - (82.0 - 1.4) * cp;
  const pctNow = cp * 98.3;
  const showStored = lt > collapseStart + 0.2;
  const showGet = lt > 6.4;

  return (
    <Scene start={S} end={E} fade={0.5}>
      <Sprite start={S + 0.0} end={E}>
        <Caption
          kicker="Delta compression"
          text={
            <>
              Transparent <span style={{ color: C.clay }}>xdelta3</span> diff vs a baseline.
            </>
          }
        />
      </Sprite>

      <Card
        x={370}
        y={300}
        w={1180}
        h={130}
        style={{ background: C.dark, border: `1px solid ${C.darkLine}`, borderRadius: 14 }}
      >
        <div style={{ display: 'flex', gap: 9, padding: '18px 0 0 22px' }}>
          {['#E5685A', '#E6B14C', '#62B25A'].map((c, i) => (
            <span
              key={i}
              style={{ width: 13, height: 13, borderRadius: 7, background: c, opacity: 0.92 }}
            />
          ))}
        </div>
        <div
          style={{
            padding: '16px 26px',
            fontFamily: MONO,
            fontSize: 26,
            color: '#DCEEF0',
            letterSpacing: '0.01em',
          }}
        >
          {shown}
          {typeT < 1 && <span style={{ opacity: Math.floor(lt * 2) % 2 ? 1 : 0.1 }}>▋</span>}
          {uploaded && (
            <span style={{ color: '#5FD0C4', marginLeft: 14, opacity: ease((lt - 2.5) / 0.4) }}>
              ✓ 200 OK
            </span>
          )}
        </div>
      </Card>

      <div style={{ position: 'absolute', left: 370, top: 506, width: 1180 }}>
        <div
          style={{
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'flex-end',
            marginBottom: 16,
          }}
        >
          <div style={{ fontFamily: MONO, fontSize: 22, color: C.muted, letterSpacing: '0.02em' }}>
            releases/v2.zip
          </div>
          <div
            style={{
              fontFamily: SERIF,
              fontSize: 30,
              color: C.ink,
              fontWeight: 600,
              fontVariantNumeric: 'tabular-nums',
            }}
          >
            {sizeNow.toFixed(sizeNow < 10 ? 1 : 0)} MB
          </div>
        </div>

        <div style={{ position: 'relative', height: 72 }}>
          <div
            style={{
              position: 'absolute',
              left: 0,
              top: 0,
              width: fullW,
              height: 72,
              border: `2px dashed ${C.claySoft}`,
              borderRadius: 12,
              opacity: cp * 0.9,
            }}
          />
          <div
            style={{
              position: 'absolute',
              left: 0,
              top: 0,
              width: barW,
              height: 72,
              background: `linear-gradient(180deg, ${C.clay}, #0B7E87)`,
              borderRadius: 12,
              boxShadow: '0 10px 28px -12px rgba(14,154,164,0.6)',
            }}
          />
          {showStored && (
            <div
              style={{
                position: 'absolute',
                left: barW + 22,
                top: 18,
                opacity: ease((lt - collapseStart - 0.2) / 0.5),
                fontFamily: SANS,
                fontSize: 21,
                color: C.clay,
                fontWeight: 600,
              }}
            >
              ← only the changed bytes stored
            </div>
          )}
        </div>

        <div
          style={{
            marginTop: 18,
            fontFamily: MONO,
            fontSize: 17,
            color: C.muted,
            letterSpacing: '0.01em',
            opacity: ease((lt - collapseStart - 0.1) / 0.6),
          }}
        >
          stored as a delta when it beats the 50% threshold · else passes through untouched
        </div>

        <div style={{ display: 'flex', gap: 56, marginTop: 26, alignItems: 'center' }}>
          <Stat
            label="Stored on backend"
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
          <div
            style={{
              opacity: ease((lt - 6.4) / 0.6),
              transform: `translateX(${(1 - ease((lt - 6.4) / 0.6)) * 16}px)`,
              display: showGet ? 'flex' : 'none',
              alignItems: 'center',
              gap: 12,
              fontFamily: SANS,
              fontSize: 23,
              color: C.muted,
              fontWeight: 500,
            }}
          >
            <span
              style={{
                fontFamily: MONO,
                fontSize: 18,
                color: C.green,
                background: '#DEF0ED',
                padding: '6px 12px',
                borderRadius: 8,
                fontWeight: 600,
              }}
            >
              GET
            </span>
            rebuilt byte-identical · SHA-256 verified
          </div>
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
  return (
    <Scene start={S} end={E} fade={0.5} drift={-14}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Deltaspaces"
          text={
            <>
              Files sharing a prefix share <span style={{ color: C.clay }}>one baseline.</span>
            </>
          }
        />
      </Sprite>
      <Card x={460} y={340} w={1000} h={500}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 14,
            padding: '28px 36px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <svg width="26" height="26" viewBox="0 0 24 24" fill="none">
            <path d="M3 6h6l2 2h10v11H3z" stroke={C.clay} strokeWidth="1.7" />
          </svg>
          <span style={{ fontFamily: MONO, fontSize: 25, color: C.ink, fontWeight: 600 }}>
            deltaspace
          </span>
          <span style={{ fontFamily: MONO, fontSize: 25, color: C.clay, fontWeight: 600 }}>
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
                gap: 22,
                padding: '28px 36px',
                borderBottom: i < rows.length - 1 ? `1px solid ${C.lineSoft}` : 'none',
                opacity: o,
                transform: `translateX(${(1 - o) * 22}px)`,
                background: isBase ? C.clayBg + '88' : 'transparent',
              }}
            >
              <span
                style={{ fontFamily: MONO, fontSize: 24, color: C.ink, fontWeight: 600, width: 130 }}
              >
                {r.n}
              </span>
              <span
                style={{
                  fontFamily: MONO,
                  fontSize: 16,
                  fontWeight: 600,
                  letterSpacing: '0.06em',
                  textTransform: 'uppercase',
                  color: isBase ? C.slate : C.clay,
                  background: isBase ? C.slateBg : C.clayBg,
                  padding: '7px 14px',
                  borderRadius: 8,
                }}
              >
                {r.tag}
              </span>
              <span style={{ flex: 1, fontFamily: SANS, fontSize: 19, color: C.muted }}>
                {r.sub}
              </span>
              <span
                style={{
                  fontFamily: SERIF,
                  fontSize: 30,
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
          top: 870,
          left: 0,
          right: 0,
          textAlign: 'center',
          fontFamily: MONO,
          fontSize: 18,
          color: C.muted,
          opacity: ease((lt - 1.4) / 0.6),
        }}
      >
        the first upload seeds the baseline — later versions are stored as diffs
      </div>
    </Scene>
  );
}

// ── SCENE 4 — DROP-IN S3 (18.6–23.2) ──────────────────────────────────────────
function SceneDropIn() {
  const S = 18.6;
  const E = 23.2;
  const { time } = useTimeline();
  const lt = time - S;
  const flow = (lt % 1.6) / 1.6;
  const nodes = [
    {
      title: 'S3 clients',
      lines: ['aws-cli · SDKs', 'presigned URLs'],
      x: 250,
      brand: false,
    },
    {
      title: 'DeltaGlider',
      lines: ['delta · reconstruct', 'SigV4 · cache'],
      x: 810,
      brand: true,
    },
    {
      title: 'Backend',
      lines: ['Filesystem', 'S3 / MinIO'],
      x: 1370,
      brand: false,
    },
  ];
  return (
    <Scene start={S} end={E} fade={0.5} drift={-10}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="S3-compatible"
          text={
            <>
              Drop-in. <span style={{ color: C.clay }}>Your clients never know.</span>
            </>
          }
        />
      </Sprite>
      <svg width={W} height={H} style={{ position: 'absolute', inset: 0, pointerEvents: 'none' }}>
        {[
          [550, 760],
          [1110, 1320],
        ].map(([x1, x2], i) => {
          const y = 500;
          const o = ease((lt - 0.5 - i * 0.2) / 0.5);
          const dx = x1 + (x2 - x1) * ((flow + i * 0.5) % 1);
          return (
            <g key={i}>
              <line
                x1={x1}
                y1={y}
                x2={x2}
                y2={y}
                stroke={C.clay}
                strokeWidth="2.5"
                strokeDasharray="2 9"
                strokeLinecap="round"
                opacity={o * 0.6}
              />
              {lt > 0.8 ? <circle cx={dx} cy={y} r="6" fill={C.clay} opacity={o} /> : null}
            </g>
          );
        })}
      </svg>
      {nodes.map((n, i) => {
        const o = ease((lt - 0.3 - i * 0.22) / 0.5);
        return (
          <Card
            key={i}
            x={n.x}
            y={400}
            w={300}
            h={200}
            style={{
              opacity: o,
              transform: `translateY(${(1 - o) * 18}px)`,
              display: 'flex',
              flexDirection: 'column',
              alignItems: 'center',
              justifyContent: 'center',
              gap: 12,
              border: n.brand ? `2px solid ${C.clay}` : `1px solid ${C.line}`,
            }}
          >
            {n.brand ? (
              <Logo size={26} />
            ) : (
              <div style={{ fontFamily: SANS, fontSize: 28, fontWeight: 700, color: C.ink }}>
                {n.title}
              </div>
            )}
            {n.lines.map((l, j) => (
              <div key={j} style={{ fontFamily: MONO, fontSize: 18, color: C.muted }}>
                {l}
              </div>
            ))}
          </Card>
        );
      })}
      <div
        style={{
          position: 'absolute',
          top: 700,
          left: 0,
          right: 0,
          display: 'flex',
          justifyContent: 'center',
          gap: 14,
          flexWrap: 'wrap',
          opacity: ease((lt - 1.2) / 0.6),
          maxWidth: 1300,
          margin: '0 auto',
        }}
      >
        {['PutObject', 'GetObject', 'Multipart', 'ListObjectsV2', 'Range', 'Conditional', 'CopyObject'].map(
          (op, i) => (
            <span
              key={i}
              style={{
                fontFamily: MONO,
                fontSize: 19,
                color: C.ink,
                background: C.paper,
                border: `1px solid ${C.line}`,
                padding: '11px 20px',
                borderRadius: 10,
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
  return (
    <Scene start={S} end={E} fade={0.5} drift={-12}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Access control"
          text={
            <>
              Per-user IAM, <span style={{ color: C.clay }}>fine-grained ABAC.</span>
            </>
          }
        />
      </Sprite>
      <Card x={420} y={340} w={1080} h={470}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            padding: '24px 34px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <div style={{ fontFamily: SANS, fontSize: 24, fontWeight: 600, color: C.ink }}>
            Access policy
          </div>
          <div style={{ display: 'flex', gap: 12, opacity: ease((lt - 0.3) / 0.5) }}>
            {['SigV4', 'Presigned URLs', 'ABAC'].map((b, i) => (
              <span
                key={i}
                style={{
                  fontFamily: MONO,
                  fontSize: 16,
                  color: C.slate,
                  background: C.slateBg,
                  padding: '8px 14px',
                  borderRadius: 9,
                  fontWeight: 600,
                  letterSpacing: '0.02em',
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
                padding: '22px 34px',
                borderBottom: `1px solid ${C.lineSoft}`,
                opacity: o,
                transform: `translateX(${(1 - o) * 22}px)`,
              }}
            >
              <div
                style={{
                  width: 54,
                  height: 54,
                  borderRadius: 27,
                  flexShrink: 0,
                  background: i === 0 ? C.clayBg : C.slateBg,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  fontFamily: SANS,
                  fontWeight: 700,
                  fontSize: 22,
                  color: i === 0 ? C.clay : C.slate,
                }}
              >
                {u.name[0].toUpperCase()}
              </div>
              <div style={{ flex: 1 }}>
                <div style={{ fontFamily: MONO, fontSize: 23, color: C.ink, fontWeight: 600 }}>
                  {u.name}
                </div>
                <div style={{ fontFamily: SANS, fontSize: 18, color: C.muted, marginTop: 3 }}>
                  {u.role}
                </div>
              </div>
              <div
                style={{
                  fontFamily: MONO,
                  fontSize: 18,
                  color: C.ink,
                  background: C.cream,
                  padding: '10px 16px',
                  borderRadius: 10,
                  border: `1px solid ${C.line}`,
                }}
              >
                {u.tag}
              </div>
            </div>
          );
        })}
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 12,
            padding: '20px 34px',
            fontFamily: SANS,
            fontSize: 19,
            color: C.muted,
            opacity: ease((lt - 1.5) / 0.6),
          }}
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none">
            <rect x="5" y="11" width="14" height="9" rx="2" fill={C.green} />
            <path d="M8 11V8a4 4 0 0 1 8 0v3" stroke={C.green} strokeWidth="2" />
          </svg>
          IAM users stored in an encrypted SQLCipher database
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
  const rows = [
    { n: 'v3.zip', t: 'delta', real: '82 MB', stored: '0.9 MB', c: C.clay },
    { n: 'v2.zip', t: 'delta', real: '82 MB', stored: '1.4 MB', c: C.clay },
    { n: 'v1.zip', t: 'baseline', real: '82 MB', stored: '82 MB', c: C.slate },
    { n: 'logo.png', t: 'passthrough', real: '4.2 MB', stored: '4.2 MB', c: C.muted },
  ];
  return (
    <Scene start={S} end={E} fade={0.5} scaleFrom={1.02} scaleTo={1.0}>
      <Sprite start={S} end={E}>
        <Caption
          kicker="Built-in UI"
          text={
            <>
              Browse &amp; monitor — <span style={{ color: C.clay }}>served at /_/</span>
            </>
          }
        />
      </Sprite>
      <Card x={360} y={320} w={1200} h={540} style={{ overflow: 'hidden' }}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 14,
            padding: '22px 30px',
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <span style={{ fontFamily: MONO, fontSize: 19, color: C.muted }}>builds</span>
          <span style={{ color: C.line }}>/</span>
          <span style={{ fontFamily: MONO, fontSize: 19, color: C.ink, fontWeight: 600 }}>
            releases
          </span>
          <div style={{ flex: 1 }} />
          <span style={{ fontFamily: SANS, fontSize: 17, color: C.green, fontWeight: 600 }}>
            ↓ 98% storage saved
          </span>
        </div>
        <div
          style={{
            display: 'flex',
            padding: '14px 30px',
            fontFamily: MONO,
            fontSize: 14,
            letterSpacing: '0.1em',
            textTransform: 'uppercase',
            color: C.muted,
            borderBottom: `1px solid ${C.lineSoft}`,
          }}
        >
          <div style={{ flex: 1 }}>Object</div>
          <div style={{ width: 170 }}>Strategy</div>
          <div style={{ width: 150, textAlign: 'right' }}>Logical</div>
          <div style={{ width: 160, textAlign: 'right' }}>On backend</div>
        </div>
        {rows.map((r, i) => {
          const o = ease((lt - 0.4 - i * 0.18) / 0.45);
          return (
            <div
              key={i}
              style={{
                display: 'flex',
                alignItems: 'center',
                padding: '20px 30px',
                borderBottom: i < rows.length - 1 ? `1px solid ${C.lineSoft}` : 'none',
                opacity: o,
                transform: `translateY(${(1 - o) * 12}px)`,
              }}
            >
              <div style={{ flex: 1, display: 'flex', alignItems: 'center', gap: 14 }}>
                <svg width="22" height="22" viewBox="0 0 24 24" fill="none">
                  <path d="M6 3h8l4 4v14H6z" stroke={C.muted} strokeWidth="1.6" />
                  <path d="M14 3v4h4" stroke={C.muted} strokeWidth="1.6" />
                </svg>
                <span style={{ fontFamily: MONO, fontSize: 21, color: C.ink }}>{r.n}</span>
              </div>
              <div style={{ width: 170 }}>
                <span
                  style={{
                    fontFamily: MONO,
                    fontSize: 15,
                    fontWeight: 600,
                    color: r.c,
                    background:
                      r.c === C.muted ? C.lineSoft : r.c === C.slate ? C.slateBg : C.clayBg,
                    padding: '6px 12px',
                    borderRadius: 7,
                    letterSpacing: '0.03em',
                  }}
                >
                  {r.t}
                </span>
              </div>
              <div
                style={{
                  width: 150,
                  textAlign: 'right',
                  fontFamily: MONO,
                  fontSize: 19,
                  color: C.muted,
                }}
              >
                {r.real}
              </div>
              <div
                style={{
                  width: 160,
                  textAlign: 'right',
                  fontFamily: MONO,
                  fontSize: 19,
                  color: r.stored === r.real ? C.muted : C.clay,
                  fontWeight: 600,
                }}
              >
                {r.stored}
              </div>
            </div>
          );
        })}
      </Card>
      <div
        style={{
          position: 'absolute',
          top: 880,
          left: 0,
          right: 0,
          textAlign: 'center',
          fontFamily: MONO,
          fontSize: 18,
          color: C.muted,
          opacity: ease((lt - 1.6) / 0.6),
        }}
      >
        live dashboards for cache, compression &amp; HTTP traffic — no extra containers
      </div>
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
          gap: 30,
        }}
      >
        <div style={{ opacity: logoO, transform: `translateY(${(1 - logoO) * 14}px)` }}>
          <Logo size={64} />
        </div>
        <div
          style={{
            fontFamily: SERIF,
            fontSize: 40,
            fontWeight: 500,
            color: C.ink,
            opacity: tagO,
            transform: `translateY(${(1 - tagO) * 12}px)`,
            textAlign: 'center',
            maxWidth: 1150,
            lineHeight: 1.25,
            letterSpacing: '-0.015em',
          }}
        >
          A drop-in S3 proxy that delta-compresses
          <br />
          versioned binaries. Storage drops <span style={{ color: C.clay }}>60–95%.</span>
        </div>
        <div
          style={{
            fontFamily: MONO,
            fontSize: 20,
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
          'radial-gradient(120% 120% at 50% 40%, transparent 55%, rgba(12,40,48,0.06) 100%)',
      }}
    />
  );
}

function ProgressDots() {
  const { time } = useTimeline();
  const marks = [0, 4.6, 13.4, 18.6, 23.2, 28.2, 33.4];
  let active = 0;
  marks.forEach((m, i) => {
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
        gap: 14,
      }}
    >
      {marks.map((_m, i) => (
        <div
          key={i}
          style={{
            width: i === active ? 34 : 9,
            height: 9,
            borderRadius: 5,
            background: i === active ? C.clay : C.line,
            transition: 'width 0.4s, background 0.4s',
          }}
        />
      ))}
    </div>
  );
}

// ── prefers-reduced-motion ────────────────────────────────────────────────────
function usePrefersReducedMotion() {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    setReduced(mq.matches);
    const on = () => setReduced(mq.matches);
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, []);
  return reduced;
}

// ── Hero walkthrough: cover-scaled canvas + autoplay loop + play/pause ─────────
const DURATION = 37;

export default function HeroWalkthrough() {
  const reduced = usePrefersReducedMotion();
  const wrapRef = useRef<HTMLDivElement>(null);
  const rafRef = useRef<number | null>(null);
  const lastTsRef = useRef<number | null>(null);
  const [time, setTime] = useState(0);
  const [playing, setPlaying] = useState(true);
  const [scale, setScale] = useState(1);

  // Cover-scale the 1920×1080 canvas to fill the plate (crop, anchor left/top).
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const measure = () => {
      const s = Math.max(el.clientWidth / W, el.clientHeight / H);
      setScale(s > 0 ? s : 1);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Autoplay RAF loop (loops at DURATION). Paused state stops it.
  useEffect(() => {
    if (!playing || reduced) {
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
  }, [playing, reduced]);

  const ctx = useMemo(() => ({ time, duration: DURATION }), [time]);

  // Under reduced-motion render nothing — the screenshot slideshow fallback
  // (gated by CSS in index.astro) takes the plate instead.
  if (reduced) return null;

  return (
    <div ref={wrapRef} className="hero-walkthrough">
      <div
        className="hero-walkthrough__canvas"
        style={{ width: W, height: H, transform: `scale(${scale})`, background: C.cream }}
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
  );
}
