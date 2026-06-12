/**
 * HeroSavingsPanel — the dashboard "money shot", v2.
 *
 * Redesigned after a three-expert pass (information design / visual
 * craft / contrarian). The one story this panel tells in two seconds:
 *
 *     "You uploaded 1.4 TB. You're storing 96 GB."
 *
 * Layout: two zones over a full-width proof.
 *   - LEFT — the magnitude as a percentage ("270% smaller") at billboard size in gradient
 *     ink. Unbounded and visceral where the old % hero was capped and
 *     flat. The % survives as a green pill underneath.
 *   - RIGHT — the before/after proof: a dashed ghost bar ("without
 *     DeltaGlider", always full width) above the real bar ("with"),
 *     where the dense teal KEPT slice is dwarfed by the luminous green
 *     SAVED field. Green = money not spent; teal-ink = bytes kept.
 *     Then the dollar line: $/mo AND $/yr.
 *   - FOOTER — one quiet strip of derived facts (objects · buckets ·
 *     biggest single save · reference share).
 *
 * Empty state (no scans yet): never renders 0% / $0.00. Instead a
 * designed "Ready to measure" composition with an EXAMPLE-labelled
 * illustration of the two bars and a primary scan CTA.
 *
 * Animation: one choreographed timeline (ghost paints → bars + numbers
 * count together → settle pulse → shimmer). Session-gated, honours
 * prefers-reduced-motion, suppressed while live SSE frames stream.
 */

import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import {
  LazyMotion,
  domAnimation,
  m,
  useMotionValue,
  useTransform,
  animate,
} from 'motion/react';
import { SettingOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { formatBytes, clamp } from '../utils';
import { heroFontPx } from '../heroNumberSize';
import { useFixedOverlayPosition } from '../useFixedOverlayPosition';

/** Cost rate presets. */
const COST_PRESETS = [
  { label: 'AWS S3', rate: 0.023 },
  { label: 'AWS S3 IA', rate: 0.0125 },
  { label: 'Hetzner', rate: 0.00524 },
  { label: 'Backblaze', rate: 0.006 },
  { label: 'Cloudflare R2', rate: 0 },
] as const;

const SESSION_GATE_KEY = 'dgp-hero-animated';
const ANIMATION_MS = 1400;
const EASE = [0.16, 1, 0.3, 1] as const;

interface Props {
  totalOriginal: number;
  totalStored: number;
  savingsPercent: number;
  monthlySavings: number;
  costRate: number;
  onChangeCostRate: (rate: number) => void;
  /** Footer facts (computed upstream). */
  totalObjects: number;
  bucketCount: number;
  biggestSave: { bucket: string; bytes: number } | null;
  /** Reference bytes as a share of stored bytes (0..1), null if unknown. */
  referenceShare: number | null;
  /** Buckets without a cached scan — powers the empty-state CTA. */
  unscannedCount: number;
  onScanMissing: () => void;
  /** When a scan is streaming, suppress count-up so values track live frames. */
  liveScanning: boolean;
}

export default function HeroSavingsPanel(props: Props) {
  return (
    <LazyMotion features={domAnimation} strict>
      <HeroInner {...props} />
    </LazyMotion>
  );
}

function HeroInner({
  totalOriginal,
  totalStored,
  savingsPercent,
  monthlySavings,
  costRate,
  onChangeCostRate,
  totalObjects,
  bucketCount,
  biggestSave,
  referenceShare,
  unscannedCount,
  onScanMissing,
  liveScanning,
}: Props) {
  const colors = useColors();
  const hasData = totalOriginal > 0;

  // ─── Motion values ────────────────────────────────────────────────
  const ratioMV = useMotionValue(1);
  const dollarMV = useMotionValue(0);
  const savedWidthMV = useMotionValue(0); // 0..100 — width of the SAVED field

  const compressionRatio = totalStored > 0 ? totalOriginal / totalStored : 0;
  const useRatioLead = compressionRatio >= 1.05;
  const targetRatio = Math.max(1, compressionRatio);
  const targetPercent = clamp(savingsPercent, 0, 100);
  const targetSavedWidth = clamp(savingsPercent, 0, 100);
  const targetDollars = Math.max(0, monthlySavings);

  // GOLDEN RULE: magnitudes are expressed as percentages, never as "N×".
  // The lead figure is the multiplier × 100 — "2.7× smaller" renders as
  // "270% smaller".
  const ratioDisplay = useTransform(ratioMV, v =>
    Math.round(v * 100).toLocaleString(),
  );
  const dollarDisplay = useTransform(dollarMV, v => v.toFixed(2));
  const percentLead = useTransform(ratioMV, () => targetPercent.toFixed(1));
  const keptWidth = useTransform(savedWidthMV, v => `max(${100 - v}%, 3px)`);
  const savedWidth = useTransform(savedWidthMV, v => `${v}%`);
  const seamLeft = useTransform(savedWidthMV, v => `calc(${100 - v}% - 1px)`);

  const [settled, setSettled] = useState(false);

  useEffect(() => {
    if (!hasData) return;
    const prefersReduced =
      typeof window !== 'undefined' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const alreadyPlayed =
      typeof sessionStorage !== 'undefined' &&
      sessionStorage.getItem(SESSION_GATE_KEY) === '1';
    const skipAnim = prefersReduced || alreadyPlayed || liveScanning;

    if (skipAnim) {
      ratioMV.set(targetRatio);
      dollarMV.set(targetDollars);
      savedWidthMV.set(targetSavedWidth);
      setSettled(true);
      return;
    }

    ratioMV.set(1);
    dollarMV.set(0);
    savedWidthMV.set(0);
    setSettled(false);

    const opts = { duration: ANIMATION_MS / 1000, ease: EASE };
    const ctrl1 = animate(ratioMV, targetRatio, opts);
    const ctrl2 = animate(dollarMV, targetDollars, opts);
    const ctrl3 = animate(savedWidthMV, targetSavedWidth, opts);

    const settleTimer = window.setTimeout(() => {
      setSettled(true);
      if (typeof sessionStorage !== 'undefined') {
        sessionStorage.setItem(SESSION_GATE_KEY, '1');
      }
    }, ANIMATION_MS + 150);

    return () => {
      ctrl1.stop();
      ctrl2.stop();
      ctrl3.stop();
      window.clearTimeout(settleTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hasData, targetRatio, targetDollars, targetSavedWidth, liveScanning]);

  // ─── Cost popover ────────────────────────────────────────────────
  const [showCostConfig, setShowCostConfig] = useState(false);
  const cogRef = useRef<HTMLButtonElement>(null);
  const [popoverEl, setPopoverEl] = useState<HTMLDivElement | null>(null);
  const pos = useFixedOverlayPosition(cogRef, showCostConfig, {
    overlayEl: popoverEl,
    offset: 6,
  });
  const POPOVER_WIDTH = 220;
  const popoverLeft = cogRef.current
    ? Math.max(8, cogRef.current.getBoundingClientRect().right - POPOVER_WIDTH)
    : pos.left;

  useEffect(() => {
    if (!showCostConfig) return;
    const handlePointerDown = (e: PointerEvent) => {
      const target = e.target as Node | null;
      if (!target) return;
      if (cogRef.current?.contains(target)) return;
      if (popoverEl?.contains(target)) return;
      setShowCostConfig(false);
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setShowCostConfig(false);
    };
    document.addEventListener('pointerdown', handlePointerDown, true);
    document.addEventListener('keydown', handleKey);
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown, true);
      document.removeEventListener('keydown', handleKey);
    };
  }, [showCostConfig, popoverEl]);

  // ─── Adaptive numeral sizing ─────────────────────────────────────
  // Size from the FINAL target string (not the animated value) so the
  // count-up never reflows the font mid-animation. The column width is
  // tracked with a ResizeObserver; the pure math lives in
  // heroNumberSize.ts. Hoisted above the early return (rules of hooks),
  // like the keptWidth/savedWidth transforms above.
  const leadString = useRatioLead
    ? Math.round(targetRatio * 100).toLocaleString()
    : targetPercent.toFixed(1);
  const [leftCol, setLeftCol] = useState<HTMLDivElement | null>(null);
  const [leftColWidth, setLeftColWidth] = useState(0);
  useEffect(() => {
    if (!leftCol) return;
    const ro = new ResizeObserver(entries => {
      const w = entries[0]?.contentRect.width ?? 0;
      if (w > 0) {
        setLeftColWidth(prev => (Math.abs(prev - w) < 0.5 ? prev : w));
      }
    });
    ro.observe(leftCol);
    return () => ro.disconnect();
  }, [leftCol]);
  const heroFontSize = heroFontPx(leadString.length, leftColWidth);

  // ─── Empty state ─────────────────────────────────────────────────
  if (!hasData) {
    return (
      <EmptyHero
        colors={colors}
        bucketCount={bucketCount}
        unscannedCount={unscannedCount}
        onScanMissing={onScanMissing}
      />
    );
  }

  const savedBytes = Math.max(0, totalOriginal - totalStored);
  const keptPct = 100 - targetSavedWidth;
  const yearlySavings = targetDollars * 12;

  const monoLabel: React.CSSProperties = {
    fontFamily: 'var(--font-mono)',
    fontSize: 10,
    fontWeight: 600,
    letterSpacing: '0.08em',
    textTransform: 'uppercase',
  };

  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'minmax(240px, 0.9fr) 1.6fr',
        gap: 'clamp(20px, 2.5vw, 44px)',
        alignItems: 'center',
        height: '100%',
        position: 'relative',
        overflow: 'hidden',
        fontFamily: 'var(--font-ui)',
      }}
    >
      {/* Green haze behind the numeral. */}
      <div
        aria-hidden
        style={{
          position: 'absolute',
          inset: 0,
          pointerEvents: 'none',
          background: `radial-gradient(120% 90% at 14% 35%, ${colors.GLOW_GREEN.replace(/[\d.]+\)$/, '0.10)')} 0%, transparent 62%)`,
          animation: 'dgHeroHazePulse 6s ease-in-out infinite',
        }}
      />

      {/* ── LEFT: the multiplier ─────────────────────────────────── */}
      <div
        ref={setLeftCol}
        style={{
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          gap: 10,
          position: 'relative',
          zIndex: 1,
          minWidth: 0,
        }}
      >
        <div
          style={{
            fontSize: 11,
            fontWeight: 800,
            letterSpacing: '0.16em',
            textTransform: 'uppercase',
            color: colors.SAVED_TEXT,
          }}
        >
          Storage saved
        </div>
        <m.div
          aria-live="polite"
          aria-label={
            useRatioLead
              ? `${Math.round(targetRatio * 100)} percent smaller`
              : `${targetPercent.toFixed(1)} percent storage saved`
          }
          animate={settled ? { scale: [1, 1.015, 1] } : undefined}
          transition={{ duration: 0.32, ease: 'easeOut' }}
          style={{
            display: 'flex',
            alignItems: 'baseline',
            fontWeight: 800,
            // Adaptive: sized for the FINAL string so "1%" renders huge
            // and "100,000%" still fits the column (heroNumberSize.ts).
            fontSize: heroFontSize,
            lineHeight: 0.95,
            letterSpacing: '-0.045em',
            fontVariantNumeric: 'tabular-nums',
            background: colors.HERO_NUM_GRADIENT,
            backgroundSize: '200% 200%',
            WebkitBackgroundClip: 'text',
            backgroundClip: 'text',
            color: 'transparent',
            animation: 'dgNumSheen 8s ease-in-out infinite',
          }}
        >
          {useRatioLead ? (
            <>
              <m.span>{ratioDisplay}</m.span>
              <span style={{ fontSize: '0.42em', fontWeight: 800, marginLeft: 2 }}>%</span>
            </>
          ) : (
            <>
              <m.span>{percentLead}</m.span>
              <span style={{ fontSize: '0.38em', fontWeight: 800, marginLeft: 4 }}>%</span>
            </>
          )}
        </m.div>
        <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 6 }}>
          {useRatioLead && (
            <div style={{ fontSize: 14, fontWeight: 600, color: colors.TEXT_SECONDARY }}>
              smaller on disk
            </div>
          )}
          <div
            style={{
              padding: '4px 14px',
              borderRadius: 999,
              fontSize: 13,
              fontWeight: 700,
              fontVariantNumeric: 'tabular-nums',
              color: colors.SAVED_TEXT_DEEP,
              background: `${colors.ACCENT_GREEN}14`,
              border: `1px solid ${colors.ACCENT_GREEN}3a`,
            }}
          >
            {targetPercent.toFixed(1)}% saved
          </div>
        </div>
      </div>

      {/* ── RIGHT: before/after proof + dollars ──────────────────── */}
      <div
        style={{
          display: 'flex',
          flexDirection: 'column',
          gap: 14,
          zIndex: 1,
          minWidth: 0,
        }}
      >
        {/* WITHOUT — dashed ghost, always full width. */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 5 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline' }}>
            <span style={{ ...monoLabel, color: colors.TEXT_FAINT }}>Without DeltaGlider</span>
            <span
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'clamp(14px, 1.2vw, 18px)',
                fontWeight: 600,
                color: colors.TEXT_SECONDARY,
                fontVariantNumeric: 'tabular-nums',
              }}
            >
              {formatBytes(totalOriginal)}
            </span>
          </div>
          <m.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ delay: 0.14, duration: 0.3 }}
            style={{
              height: 22,
              borderRadius: 7,
              border: `1.5px dashed ${colors.TEXT_FAINT}`,
              opacity: 0.55,
              background: `repeating-linear-gradient(45deg, ${colors.TEXT_FAINT}22 0 1px, transparent 1px 8px)`,
            }}
            title={`Original: ${formatBytes(totalOriginal)}`}
          />
        </div>

        {/* WITH — kept slice (dense teal) + saved field (luminous green). */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 5 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline' }}>
            <span style={{ ...monoLabel, color: colors.KEPT_TEXT }}>With DeltaGlider</span>
            <span
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'clamp(14px, 1.2vw, 18px)',
                fontWeight: 700,
                color: colors.KEPT_TEXT,
                fontVariantNumeric: 'tabular-nums',
              }}
            >
              {formatBytes(totalStored)}
            </span>
          </div>
          <div
            role="img"
            aria-label={`${formatBytes(totalStored)} stored from ${formatBytes(totalOriginal)} original — ${targetPercent.toFixed(1)} percent saved`}
            style={{
              position: 'relative',
              width: '100%',
              height: 40,
              background: colors.BAR_TRACK,
              border: `1px solid ${colors.BORDER}`,
              borderRadius: 10,
              overflow: 'hidden',
              display: 'flex',
              boxShadow: colors.INSET_SHADOW,
            }}
          >
            {/* Kept slice — dense, matte. */}
            <m.div
              style={{
                width: keptWidth,
                background: colors.BAR_KEPT,
                boxShadow:
                  'inset 0 1px 0 rgba(255,255,255,0.18), inset 0 -1px 0 rgba(0,0,0,0.18)',
                display: 'flex',
                alignItems: 'center',
                overflow: 'hidden',
                flexShrink: 0,
              }}
              title={`Kept on disk: ${formatBytes(totalStored)}`}
            >
              {keptPct >= 14 && (
                <span
                  style={{
                    ...monoLabel,
                    color: 'rgba(255,255,255,0.92)',
                    textShadow: '0 1px 2px rgba(0,0,0,0.25)',
                    padding: '0 12px',
                    whiteSpace: 'nowrap',
                  }}
                >
                  kept {formatBytes(totalStored)}
                </span>
              )}
            </m.div>
            {/* Saved field — luminous, striped, glowing. */}
            <m.div
              style={{
                width: savedWidth,
                position: 'relative',
                background: `repeating-linear-gradient(45deg, rgba(255,255,255,0.10) 0 1px, transparent 1px 7px), ${colors.BAR_SAVED}`,
                boxShadow: `inset 0 1px 0 rgba(255,255,255,0.35), 0 0 18px ${colors.GLOW_GREEN}`,
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'flex-end',
                overflow: 'hidden',
              }}
              title={`Saved: ${formatBytes(savedBytes)}`}
            >
              {targetSavedWidth >= 22 && (
                <span
                  style={{
                    ...monoLabel,
                    color: 'rgba(255,255,255,0.95)',
                    textShadow: '0 1px 2px rgba(0,0,0,0.3)',
                    padding: '0 12px',
                    whiteSpace: 'nowrap',
                  }}
                >
                  saved {formatBytes(savedBytes)}
                </span>
              )}
              {/* Shimmer sweep after settle, then a faint slow loop. */}
              {settled && (
                <div
                  className="dg-saved-sheen"
                  aria-hidden
                  style={{
                    position: 'absolute',
                    top: 0,
                    bottom: 0,
                    width: '36%',
                    background:
                      'linear-gradient(90deg, transparent, rgba(255,255,255,0.30), transparent)',
                    animation:
                      'dgSavedSheen 1.1s cubic-bezier(0.4,0,0.2,1) 0.2s 1 both, dgSavedSheen 1.4s cubic-bezier(0.4,0,0.2,1) 4s infinite',
                    opacity: 0.85,
                  }}
                />
              )}
            </m.div>
            {/* Seam blade at the compression front. */}
            <m.div
              aria-hidden
              style={{
                position: 'absolute',
                top: 0,
                bottom: 0,
                width: 2,
                left: seamLeft,
                background: 'rgba(255,255,255,0.85)',
                boxShadow: `0 0 10px 1px ${colors.GLOW_GREEN}`,
                borderRadius: 1,
              }}
            />
          </div>
          {/* Tick ruler. */}
          <div aria-hidden style={{ position: 'relative', height: 12, marginTop: 1 }}>
            {Array.from({ length: 11 }, (_, i) => i * 10).map(t => (
              <div
                key={t}
                style={{
                  position: 'absolute',
                  left: `${t}%`,
                  top: 0,
                  width: 1,
                  height: t % 50 === 0 ? 6 : 4,
                  background: colors.TEXT_FAINT,
                  opacity: t % 50 === 0 ? 0.7 : 0.4,
                }}
              />
            ))}
            {[0, 50, 100].map(t => (
              <span
                key={t}
                style={{
                  position: 'absolute',
                  left: `${t}%`,
                  top: 5,
                  transform: t === 0 ? 'none' : t === 100 ? 'translateX(-100%)' : 'translateX(-50%)',
                  fontFamily: 'var(--font-mono)',
                  fontSize: 9,
                  fontWeight: 500,
                  color: colors.TEXT_FAINT,
                  letterSpacing: '0.04em',
                }}
              >
                {t === 0 ? '0' : `${t}%`}
              </span>
            ))}
          </div>
        </div>

        {/* Dollar line. Sub-cent savings render as "<$0.01" rather than a
            broken-looking $0.00. */}
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap' }}>
          <span
            style={{
              fontSize: 'clamp(24px, 2vw, 34px)',
              fontWeight: 800,
              color: colors.TEXT_PRIMARY,
              fontVariantNumeric: 'tabular-nums',
              letterSpacing: '-0.03em',
            }}
          >
            {targetDollars > 0 && targetDollars < 0.01 ? (
              <span>
                <span style={{ color: colors.SAVED_TEXT }}>&lt;$</span>0.01
              </span>
            ) : (
              <>
                <span style={{ color: colors.SAVED_TEXT }}>$</span>
                <m.span>{dollarDisplay}</m.span>
              </>
            )}
            <span style={{ fontSize: '0.55em', fontWeight: 600, color: colors.TEXT_MUTED }}>
              /mo
            </span>
          </span>
          {yearlySavings >= 1 && (
            <span
              style={{
                fontSize: 15,
                fontWeight: 700,
                color: colors.SAVED_TEXT_DEEP,
                fontVariantNumeric: 'tabular-nums',
              }}
            >
              ${yearlySavings >= 100 ? Math.round(yearlySavings).toLocaleString() : yearlySavings.toFixed(2)}/yr
            </span>
          )}
          <span style={{ fontSize: 12, color: colors.TEXT_MUTED, fontWeight: 500 }}>
            saved at ${costRate}/GB
          </span>
          <button
            ref={cogRef}
            onClick={() => setShowCostConfig(v => !v)}
            aria-label="Pick cost rate"
            aria-expanded={showCostConfig}
            aria-haspopup="listbox"
            title="Pick cost rate preset"
            style={{
              background: 'transparent',
              border: 'none',
              cursor: 'pointer',
              color: colors.TEXT_MUTED,
              padding: 4,
              display: 'inline-flex',
              alignItems: 'center',
              borderRadius: 4,
              transition: 'color 0.12s',
            }}
            onMouseEnter={e => (e.currentTarget.style.color = colors.TEXT_PRIMARY)}
            onMouseLeave={e => (e.currentTarget.style.color = colors.TEXT_MUTED)}
          >
            <SettingOutlined />
          </button>
        </div>
        {showCostConfig &&
          createPortal(
            <div
              ref={setPopoverEl}
              role="listbox"
              aria-label="Cost per GB/month"
              style={{
                position: 'fixed',
                top: pos.top,
                left: popoverLeft,
                width: POPOVER_WIDTH,
                zIndex: 99999,
                padding: 10,
                background: colors.BG_ELEVATED,
                border: `1px solid ${colors.BORDER}`,
                borderRadius: 8,
                boxShadow: colors.ELEV_SHADOW,
                fontFamily: 'var(--font-ui)',
                fontVariantNumeric: 'tabular-nums',
                color: colors.TEXT_PRIMARY,
              }}
            >
              <div
                style={{
                  fontSize: 10,
                  fontWeight: 700,
                  color: colors.TEXT_MUTED,
                  textTransform: 'uppercase',
                  letterSpacing: '0.06em',
                  marginBottom: 6,
                }}
              >
                Cost per GB/month
              </div>
              {COST_PRESETS.map(p => (
                <div
                  key={p.label}
                  role="option"
                  tabIndex={0}
                  aria-selected={costRate === p.rate}
                  onClick={() => {
                    onChangeCostRate(p.rate);
                    setShowCostConfig(false);
                  }}
                  onKeyDown={e => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      onChangeCostRate(p.rate);
                      setShowCostConfig(false);
                    }
                  }}
                  style={{
                    padding: '6px 8px',
                    cursor: 'pointer',
                    borderRadius: 4,
                    fontSize: 12,
                    color: costRate === p.rate ? colors.ACCENT_BLUE : colors.TEXT_PRIMARY,
                    background: costRate === p.rate ? `${colors.ACCENT_BLUE}18` : 'transparent',
                  }}
                >
                  {p.label} — ${p.rate}/GB
                </div>
              ))}
            </div>,
            document.body,
          )}
      </div>

      {/* ── FOOTER STRIP: derived facts, one quiet line ──────────── */}
      <div
        style={{
          gridColumn: '1 / -1',
          display: 'flex',
          alignItems: 'center',
          flexWrap: 'wrap',
          gap: 6,
          fontSize: 11,
          color: colors.TEXT_MUTED,
          fontVariantNumeric: 'tabular-nums',
          zIndex: 1,
          borderTop: `1px solid ${colors.BORDER}`,
          paddingTop: 10,
          marginTop: -2,
        }}
      >
        <FooterFact text={`${totalObjects.toLocaleString()} objects`} />
        <Dot colors={colors} />
        <FooterFact text={`${bucketCount} bucket${bucketCount === 1 ? '' : 's'}`} />
        {biggestSave && biggestSave.bytes > 0 && (
          <>
            <Dot colors={colors} />
            <FooterFact
              text={`biggest single save: ${biggestSave.bucket} −${formatBytes(biggestSave.bytes)}`}
            />
          </>
        )}
        {referenceShare !== null && referenceShare > 0 && referenceShare <= 1 && (
          <>
            <Dot colors={colors} />
            <FooterFact
              text={`references are ${(referenceShare * 100).toFixed(0)}% of stored — the rest is pure deltas`}
            />
          </>
        )}
        {unscannedCount > 0 && (
          <>
            <Dot colors={colors} />
            <span style={{ color: colors.ACCENT_AMBER }}>
              {unscannedCount} bucket{unscannedCount === 1 ? '' : 's'} not scanned
            </span>
          </>
        )}
      </div>
    </div>
  );
}

function FooterFact({ text }: { text: string }) {
  return <span>{text}</span>;
}

function Dot({ colors }: { colors: ReturnType<typeof useColors> }) {
  return <span style={{ color: colors.TEXT_FAINT }}>·</span>;
}

/**
 * Designed empty state — no zeros, no dead bars. A headline, a scan
 * CTA, and an honest EXAMPLE-labelled illustration of the two bars.
 */
function EmptyHero({
  colors,
  bucketCount,
  unscannedCount,
  onScanMissing,
}: {
  colors: ReturnType<typeof useColors>;
  bucketCount: number;
  unscannedCount: number;
  onScanMissing: () => void;
}) {
  const n = unscannedCount > 0 ? unscannedCount : bucketCount;
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'minmax(260px, 1fr) 1.2fr',
        gap: 'clamp(20px, 3vw, 48px)',
        alignItems: 'center',
        height: '100%',
        fontFamily: 'var(--font-ui)',
      }}
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
        <div
          style={{
            fontSize: 'clamp(20px, 2vw, 27px)',
            fontWeight: 800,
            letterSpacing: '-0.02em',
            color: colors.TEXT_PRIMARY,
            lineHeight: 1.2,
          }}
        >
          Ready to measure your savings
        </div>
        <div style={{ fontSize: 13, color: colors.TEXT_MUTED, lineHeight: 1.55, maxWidth: 380 }}>
          DeltaGlider stores versioned artifacts as binary deltas. Scan your{' '}
          {bucketCount > 0 ? `${bucketCount} ` : ''}bucket{bucketCount === 1 ? '' : 's'} to see how
          much you're really storing.
        </div>
        {n > 0 && (
          <button
            onClick={onScanMissing}
            style={{
              alignSelf: 'flex-start',
              height: 40,
              padding: '0 22px',
              borderRadius: 8,
              border: 'none',
              cursor: 'pointer',
              fontSize: 14,
              fontWeight: 700,
              fontFamily: 'var(--font-ui)',
              color: '#fff',
              background: colors.BAR_SAVED,
              boxShadow: `0 2px 12px ${colors.GLOW_GREEN}`,
            }}
          >
            Scan {n} bucket{n === 1 ? '' : 's'}
          </button>
        )}
      </div>

      <div style={{ display: 'flex', flexDirection: 'column', gap: 12, position: 'relative' }}>
        <span
          style={{
            position: 'absolute',
            top: -22,
            right: 0,
            fontSize: 9,
            fontWeight: 800,
            letterSpacing: '0.12em',
            textTransform: 'uppercase',
            color: colors.TEXT_FAINT,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 4,
            padding: '2px 7px',
          }}
        >
          Example
        </span>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: '0.08em', textTransform: 'uppercase', color: colors.TEXT_FAINT, fontFamily: 'var(--font-mono)' }}>
            Your current storage
          </span>
          <div
            style={{
              height: 22,
              borderRadius: 7,
              background: `repeating-linear-gradient(45deg, ${colors.TEXT_FAINT}33 0 1px, transparent 1px 8px)`,
              border: `1px solid ${colors.BORDER}`,
            }}
          />
        </div>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: '0.08em', textTransform: 'uppercase', color: colors.TEXT_FAINT, fontFamily: 'var(--font-mono)' }}>
            After DeltaGlider — run a scan to find out
          </span>
          <div
            style={{
              height: 22,
              width: '7%',
              minWidth: 26,
              borderRadius: 7,
              border: `1.5px dashed ${colors.ACCENT_GREEN}88`,
            }}
          />
        </div>
        <div style={{ fontSize: 11, color: colors.TEXT_MUTED }}>
          Versioned builds and backups typically end up 80–98% smaller.
        </div>
      </div>
    </div>
  );
}
