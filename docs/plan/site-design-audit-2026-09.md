# Site design audit — September 2026

Scope: the marketing site in `marketing/` (home, `/saas`, `/regulated`, `/pricing`,
`/trial`, `/case-studies`, the case study, `/404`) and the docs pages
(`/docs`, an explanation page, a reference page, a tutorial). Build: origin/main
`43cc1f77`. Screenshots: 1440 px Chromium and 393 px WebKit (iPhone 15), dark and
light, full page, plus close-ups of every hero scene and every graphic component.

The owner's brief: "some graphics look really off and random. Coherence, visual
discipline and clear visual messaging is paramount."

## Summary

The base of the site is sound: one sans family for body text, one brand hue
(cyan), a dark-first token file, and a real product story. The problems are
accumulation, not a wrong direction:

1. **The home hero has three focal points that compete.** A 64 px gradient
   headline, a perpetually rotating "orbit" of four pills, and a 37-second
   animated walkthrough on a light "paper" card with its own palette and a serif
   typeface. No single graphic says "control plane".
2. **Six typefaces are loaded.** Manrope and JetBrains Mono (the system), plus
   Spectral (serif) and Hanken Grotesk for the walkthrough, and Schibsted
   Grotesk and Spline Sans Mono for the docs.
3. **Fuchsia has no role.** It is the "reference" colour in the storage chart,
   but also the second stroke of every icon, a glow in every hero, the footer
   rule, the trial timeline and the "Delta" badge in the walkthrough. So when it
   means something, the reader cannot tell.
4. **The primary button is broken on five pages.** `.btn-primary` paints
   `--ink-100`, which is the page background in dark mode and a pale grey in
   light mode. On `/saas`, `/trial`, `/regulated`, `/pricing` and the case study
   the "primary" call to action is the weakest button on the screen. Only the
   home page and 404 use `.btn-brand`, which is the real primary.
5. **Light mode has four hard-coded dark-mode colours**: a grey slab behind the
   `/regulated` sidebar, an illegible yellow "AES-256 key" chip, a pale-pink
   "- endpoint" diff line in the drop-in panel, and Mermaid's default lavender
   and yellow diagrams in the docs.
6. **Emoji sit next to the drawn icon set** (the customer-segment list on
   `/regulated`, the workload-fit table on the home page, the `⚿` key chip).
7. **Decorative numbering and scale drift.** Numbered markers 01–06 on features
   that are not a sequence, giant clipped numerals 01–05 behind the FAQ cards,
   14 different corner radii, 43 different font sizes.

The fix is one visual system (section 2), applied tokens first. Most of the
change list is small; two items are medium (a static hero diagram, one shared bar
chart) and one is a decision for the owner (what replaces the walkthrough).

---

## 1. Inventory

Verdicts: **keep** (no change), **fix** (keep the element, correct it),
**replace** (the job is right, the form is wrong), **remove** (no message).

### Global chrome

| Element | Where | Verdict | Reason |
|---|---|---|---|
| Header (`SiteHeader`) | all | fix | "Beshu" in a lighter weight than "DeltaGlider" reads as two brands. The shell is 1100 px, but the home hero is wider (86 → 1396 px at 1440), so the logo does not align with the headline below it. |
| Theme toggle | all | keep | One icon button, same radius as other controls. |
| Footer (`SiteFooter`) | all | fix | Cyan-to-fuchsia gradient rule and three radial glows (cyan, fuchsia, cyan). Decorative; the footer is the quietest block of a page. Replace with one `--rule` line and a flat surface. |
| Brand mark (`BrandMark`) | header, footer, orbit, walkthrough | keep | The one mark. Keep it the only logo; do not add it to diagrams except as the proxy node. |
| Page background glows | every hero (`.hero` radial cyan + fuchsia) | fix | Each hero has a cyan wash top-right and a fuchsia wash bottom-left. The fuchsia wash is an unexplained purple smudge (clear on `/regulated`, `/case-studies`, `/404`, `/trial`). Keep at most one brand wash. |
| Buttons (`.btn`, `.btn-primary`, `.btn-brand`, `.btn-ghost`) | all | fix | `.btn-primary` is the bug in item 4 of the summary. Hover lifts by 2 px, scales by 2 %, and adds a glow; three effects on one action. |
| Section eyebrows (`.section-eyebrow`) | all pages | fix | Tracked ALL-CAPS label above nearly every H2, often restating the H2 ("SEE VIDEO" above "Ninety seconds, end to end"; "WORKLOAD FIT" above "Will it work on your data?"). The gap from eyebrow to H2 varies from 8 px to 50 px ("DUE DILIGENCE", "WORKLOAD FIT", "GET STARTED", "PLAYBOOK" have the large gap). |
| Product chip (`.product-chip`) | testimonial cards | fix | Stretches to the full card width, so it reads as a banner, not a tag. |

### Home (`/`)

| Element | Verdict | Reason |
|---|---|---|
| Hero badge "WRITTEN IN RUST · KUBERNETES-READY · FREE UP TO 15 TB" | fix | ALL-CAPS mono-ish meta string. The one useful fact (free up to 15 TB) is buried in the middle. Keep the content, set it in sentence case, same chip anatomy as other status chips. |
| Hero H1 gradient text | fix | Gradient text from white to cyan on dark and navy to cyan on light. The gradient is the only one on the site; it competes with the hero graphic. Solid `--ink-strong`. |
| `HeroOrbit` (four pills circling the brand mark) | **remove** | Perpetual motion that the reader did not start. The four words (security, encryption, multi-cloud, compression) repeat the bullets directly below. "Security" and "Encryption" overlap. It sits between the H1 and the lede, so it splits the headline from its explanation. On mobile it is the only graphic, so it becomes the hero message by accident. |
| `HeroWalkthrough` (37 s animated "paper" card) | **replace** | Its own palette (`#0E8BA8`, `#B5179E`, `#1FA98C`, `#EAF6F8`), its own serif (Spectral) and sans (Hanken Grotesk), macOS traffic-light dots, a light card on a dark page. Seven scenes, several of which fade to a blank card for seconds (the Access and Browser scenes are 80 % empty mid-transition). A dark vertical band covers the card's left 15 %: the `.hero-plate-fade` gradient meant for the old full-bleed screenshot now paints over the framed card. It also duplicates the real 90 s product video one screen below. |
| `DemoVideo` (poster + play button) | fix | The real product, correct job. Fix: the "90S VIDEO" mono caps label under the play button, the heavy cyan outer glow, and the poster's burned-in caption sitting under the page text. |
| `StorageScale` ("What ten releases do to a bucket") | keep, fix | The best graphic on the site: it proves the claim with proportional bars. Fix: plain bars are outlined grey boxes, while the reference is an outlined fuchsia box and deltas are 2 px cyan lines — three different mark types in one chart. Use filled bars in all three roles. Column headings are tracked caps mono. |
| Engineering-note callout ("Our artifacts are ZIPs and JARs…") | keep | Left brand rule callout; one pattern, reuse it. |
| `DropIn` (config diff + three cards) | fix | Diff panel is good. The minus line uses `#fda4af` on `rgba(244,63,94,.09)` — illegible in light mode. The three cards use a different card anatomy (icon tile left, bold run-in title) from the `ControlPlane` features below (icon tile, number, H3). |
| `ControlPlane` (six features, 01–06) | fix | Numbers 01–06 imply a sequence that does not exist. The icon tiles use cyan + fuchsia duotone strokes. The grid is 880 px wide inside a 1036 px shell, so the right column ends 150 px short of every other block. |
| Screenshot strip (three admin screens) | keep, fix | Real product. Screens are light UI on dark page — acceptable because they are screenshots, but give them one frame style (same as the case-study figure). |
| FAQ cards "The questions you'd ask anyway" | fix | Giant clipped numerals 01–05 in alternating purple and teal behind each card. Not a sequence, clipped at the card edge, and the colour alternation has no meaning. |
| `TestimonialsBlock` / `TestimonialCard` | fix | Full-width product chip (see chrome). Quote mark in brand cyan is fine. |
| `WorkloadFitTable` | fix | ✅ / ❌ emoji in the Recommend column. The only emoji on the page; renders differently per OS. |
| Terminal block ("Touch it in 60 seconds") | fix | Red/amber/green macOS traffic-light dots (hard-coded hex) and a vertical gradient. The drop-in panel above uses a filename header instead. Use that one anatomy for every code panel. |

### `/saas`

| Element | Verdict | Reason |
|---|---|---|
| Hero graphic (stacked cards, ghost "91 MB", "148 KB ENCRYPTED DELTA") | **replace** | Three nested outlines with a near-invisible "91 MB" (about 1.3:1 contrast). The reader must decode what the stack means. The home page already has the right grammar for this message: the ten-release bar chart. Use a compact version of it. |
| Hero CTAs | fix | Primary is the broken `.btn-primary`; all three buttons look equal. |
| `MechanismExplanation` stat card ("61,400%") | keep | One big number with a short label; the site's stat-card pattern. |
| Section rhythm | fix | "THE COST" eyebrow sits 16 px under the previous link, with no section break. |
| Speed table | keep | Plain table, consistent with the workload table. |
| "Verified results" panel (`CompressionResultsTable`) | fix | 60 px of empty space above the title; the title is caps; the cell values sit inside bordered pills with a second nested pill for the percentage — three levels of boxes. |
| Playbook 01–05 | fix | This IS a sequence, so numbers are right. But five cards in a three-column grid leave a hole, and the numerals are 48 px grey mono that out-weigh the step names. |
| "Next step" CTA block + 200 px empty band before the footer | fix | Broken primary button; the empty band makes the page look unfinished. |

### `/regulated`

| Element | Verdict | Reason |
|---|---|---|
| Hero diagram (Your runtime → encrypt, then upload → What the provider stores) | keep, fix | The right message in one picture. Fix: the "AES-256 key" chip is amber (`#fcd34d`) — a colour used nowhere else, illegible on light; the `▤` and `⚿` are font glyphs, not the icon set; labels are tracked caps. |
| Hero H1 (five lines at 52 px) | fix | The column is too narrow for the headline. |
| Sidebar card | fix | `rgba(15,23,42,.3)` hard-coded: in light mode it renders as a mid-grey slab. |
| `CustomerSegmentBlock` | fix | Five emoji (📈 🧸 ⚛️ 🇪🇺 👮‍♀️) as list icons. |
| Body bullets | keep | Long-form text, correct. |

### `/pricing`

| Element | Verdict | Reason |
|---|---|---|
| Calculator (`PricingCalculator`) | keep, fix | The page's one job, and it works. Fix: a hairline from the hero crosses behind the card at left and right; "YOUR NUMBERS" in caps. |
| Provider bar chart ("Monthly storage cost") | fix | Different grammar from `StorageScale`: here bars are solid, the value sits inside a chip that overflows short bars (`$30`, `$3.04` chips wider than their bars), the chart panel is a third surface colour. One chart style for the site. |
| Plan rows (Free / Commercial trial / Commercial / Enterprise / OEM) | fix | Price in brand mono at 24 px, so "Free, 30 days" and "Talk to sales" wrap onto two lines. The Commercial row has a 3 px top border as the only emphasis. |
| Amber note (`calculator.css`, `#fcd34d`) | fix | Off-palette amber; use the warning role token. |
| FAQ grid | keep | Plain two-column Q&A, fine. |

### `/trial`

| Element | Verdict | Reason |
|---|---|---|
| Hero dashboard screenshot with left mask fade | remove | A savings dashboard on a page about a support relationship. The mask fade produces the same dark smudge as the home hero. |
| Tagline "Free for 30 days · No credit card · No auto-conversion" | fix | Mono brand text for a normal sentence. |
| Three included-items cards | keep, fix | Same card anatomy as `DropIn`; single-colour glyphs after the icon fix. |
| Timeline "What the 30 days look like" | fix | The vertical line runs through the middle of the day labels ("Day 0", "Days 1–7" are cut by the line). The line is a cyan-to-fuchsia gradient. This is the page's message graphic; make it clean. |
| "×" bullets | fix | `#fda4af` pink, hard-coded, illegible on light. |

### Case studies

| Element | Verdict | Reason |
|---|---|---|
| Index card (left rule, "COMPLETE" outlined badge) | keep, fix | Badge in caps outline; use the status chip. |
| Case study stat card ("~~1.71 TB~~ → 134 GB") | keep | The page's message in one line. |
| Results table | fix | Same pill-in-pill cells as on `/saas` (shared component). Badges "Excellent" / "Fair" / "Complete" in mono. |
| Author byline "BY SIMONE SCARDUZIO, FOUNDER, BESHU TECH" | fix | Tracked caps mono. |
| Metric format | owner decision | The same article says "1,270% smaller" and "-98.9%". Both are correct; they are two different metrics. Not changed in this pass (no copy or number changes). |

### Docs

| Element | Verdict | Reason |
|---|---|---|
| Graph-paper grid background | remove | The docs are the only pages with it. It adds texture, not information, and makes the docs look like a different site. |
| Schibsted Grotesk (headings) + Spline Sans Mono (labels) | replace | Two extra families, used only in the docs. Manrope and JetBrains Mono cover both roles. |
| Layout width | fix | The docs grid is 1192 px (124 → 1316) under a 1036 px header, so the logo and the sidebar do not align. |
| Landing cards | fix | Descriptions truncate mid-word ("stored as a small del…"). Clamp by lines, not by characters. Card eyebrows "START HERE", "UNDERSTAND IT" tracked caps mono. |
| Section icon tiles | fix | Same duotone glyph issue as the home page. |
| Mermaid diagrams | fix | `theme: 'default'` in light mode: lavender boxes, yellow notes, Arial. `theme: 'dark'` in dark mode: grey notes. Neither uses the site palette or type. |
| Doc-actions row (Copy page, View .md, Open in ChatGPT…) | keep | Mono pill buttons; acceptable as a tool row. |
| Sidebar footer "● Docs track `main` · release notes" | fix | Mono sentence; set in the body face. |
| Product screenshots in docs | keep | Real UI, one frame style. |

### 404

| Element | Verdict | Reason |
|---|---|---|
| Page | keep | Correct button hierarchy (it uses `.btn-brand`). Only the global fixes apply. |

### Mobile (393 px WebKit)

The parallel mobile agent owns layout at this width. Graphic findings only:

- The hero walkthrough is hidden below 900 px, so the orbit is the only hero
  graphic on phones. Removing the orbit leaves the phone hero text-only until the
  hero diagram lands; the diagram must have a stacked (vertical) variant.
- The `/saas` stacked-card graphic takes a full screen of height on a phone and
  carries the least information per pixel on the page.
- The drop-in diff clips the endpoint lines at the right edge (horizontal scroll
  inside the panel is correct for code; keep it, but the pink minus line is
  illegible in light mode, as on desktop).

---

## 2. The visual system

### 2.1 Point of view

DeltaGlider is infrastructure that engineers must trust with their bytes. The
site's personality is **the instrument panel**: exact, measured, drawn to scale.
Every graphic is a measurement or a mechanism, never an ornament. Colour marks a
role in the data path, never a mood. One bold element per page: the graphic that
carries the page's message. Everything around it is quiet.

### 2.2 Palette — roles

Colours are named by what they mean. `theme.css` stays the only file with
literals.

| Token | Role | Dark | Light |
|---|---|---|---|
| `--bg` | Page | `#141a2b` | `#f4f7fb` |
| `--surface` (was `--bg-card`) | Card, panel | `rgba(30,41,59,.5)` | `rgba(255,255,255,.85)` |
| `--surface-sunk` | Chart well, table header | `rgba(15,23,42,.4)` | `rgba(148,163,184,.14)` |
| `--ink-strong`, `--ink`, `--ink-soft`, `--ink-faint` | Text: headline, body, secondary, meta | existing ramp | existing ramp |
| `--rule`, `--rule-strong` | Borders and dividers | existing | existing |
| `--brand` | **DeltaGlider**: the proxy, the stored delta, the primary action, links | `#22d3ee` | `#0e7490` (fill `--brand-fill`) |
| `--accent` | **The reference baseline — only.** Nothing else is fuchsia. | `#d946ef` | `#c026d3` |
| `--neutral-data` | **Plain storage**: a full copy, "today", the S3 bucket without the proxy | `#475569` | `#94a3b8` |
| `--ok` (new) | Verified, success, "Yes" | `#34d399` | `#047857` |
| `--neg` (new) | Removed line, "no", a limit | `#fb7185` | `#be123c` |
| `--warn` (new, = `--amber-ink`) | A caveat that changes the result | `#fcd34d` | `#92400e` |
| `--panel-dark-*` | Code and ciphertext panels, dark in both themes | existing | existing |

Rules:

- One brand wash per page at most (a hero radial of `--brand-tint`). No
  fuchsia washes, glows or gradients anywhere.
- No gradient text. No gradient rules.
- Every colour a component uses comes from a token that has a light-mode value.
  A component that hard-codes a hex is a bug (a source guard can check this:
  no hex or `rgb(` literals outside `theme.css`).

### 2.3 Type

Two families. **Manrope** for everything a person reads. **JetBrains Mono** only
for things a machine reads: code, commands, file and bucket names, byte sizes
and prices in charts and tables.

Scale (ratio 1.25 from 16 px):

| Role | Size | Weight | Tracking | Line height |
|---|---|---|---|---|
| Display (H1) | `clamp(2.5rem, 4.6vw, 3.75rem)` | 800 | −0.03em | 1.05 |
| H2 | `clamp(1.75rem, 3vw, 2.25rem)` | 700 | −0.02em | 1.15 |
| H3 | 1.25rem | 700 | −0.01em | 1.3 |
| Lede | 1.25rem | 400 | 0 | 1.55 |
| Body | 1.0625rem | 400 | 0 | 1.6 |
| Small / meta | 0.875rem | 500 | 0 | 1.5 |
| Label (chart axis, table header, eyebrow) | 0.8125rem | 600 | 0.01em | 1.4 |
| Stat numeral | `clamp(2.5rem, 5vw, 3.5rem)` mono | 700 | −0.02em | 1 |

- Sentence case for every label. No tracked ALL-CAPS, with no exceptions.
- Eyebrows only where they add a fact the H2 does not state (for example
  "Case study" above a case-study link). Remove the ones that restate the H2.
  Eyebrow to H2: 8 px.
- Prices in plan rows are body-face 700, not mono, and never wrap.
- Drop Spectral, Hanken Grotesk, Schibsted Grotesk and Spline Sans Mono from
  the font requests.

### 2.4 Spacing

4 px base: `4, 8, 12, 16, 24, 32, 48, 64, 96`.

- Section: 96 px top and bottom on desktop, 64 px on mobile. Every section,
  every page. A section break is either a `--rule` line or a surface change,
  not both.
- H2 → lede 12 px; lede → content 32 px; card padding 24 px; grid gap 16 px
  (cards) or 32 px (text columns).
- One shell: `--shell: 1100px` for the header, the hero and every section,
  including the home hero. The docs layout uses a wider shell, and its header
  uses the same wider shell.

### 2.5 Radius, borders, shadows

- Radius, three steps: `--r-sm: 6px` (buttons, inputs, chips, inline code),
  `--r-md: 12px` (cards, panels, code blocks, screenshots), `--r-pill: 999px`
  (status chips only). Circles only for avatars and play buttons.
- Border: 1 px `--rule` on every card and panel. Emphasis is a `--brand` border,
  never a thicker border or a top stripe.
- Shadow: `--shadow-sm` for raised controls, `--shadow-md` for floating panels
  (a screenshot, a modal). Cards on the page have no shadow. No glow shadows
  (`--shadow-glow` is removed from buttons, the video frame and stat cards).

### 2.6 Icons

- One family: `Glyph.astro`. 48 × 48 grid, 2.5 px round stroke, **one colour**
  (`currentColor`, which is `--brand`). The fuchsia second stroke goes, because
  fuchsia means "reference".
- One tile: 44 px square, `--r-md`, 1 px `--rule`, `--surface`, no glow.
- No emoji. No font glyphs (`▤`, `⚿`, `✕`) as icons. Add a glyph to the set
  instead (`key`, `file`, `check`, `cross`).
- Status marks in tables: `check` in `--ok`, `cross` in `--neg`, with the text
  label always next to the mark.

### 2.7 Diagram grammar

Every diagram on the site (hero, regulated, docs Mermaid, the walkthrough if it
stays) uses the same nouns:

| Thing | Shape | Colour |
|---|---|---|
| Client (SDK, CLI, CI) | Rounded rectangle `--r-sm`, 1 px `--rule-strong`, label in body face | neutral |
| **DeltaGlider proxy** | Rounded rectangle `--r-md`, 1.5 px `--brand` border, `--brand-tint` fill, brand mark at left | brand |
| Backend / bucket | Rounded rectangle with a lid line across the top (the bucket icon), 1 px `--rule-strong`; provider names in mono | neutral |
| Full object / full copy | Filled bar, length ∝ size | `--neutral-data` |
| **Reference baseline** | Filled bar | `--accent` |
| **Delta** | Filled bar, 2 px minimum length | `--brand` |
| Ciphertext | Mono hex on `--panel-dark-bg` | panel-dark |
| Request / data flow | 1.5 px line, `--ink-faint`, small filled arrowhead; label in mono 0.8125rem above the line | neutral |
| Key | `key` glyph | brand |

Lines are straight or right-angled. No dashed orbits, no ellipses, no 3D.
Mermaid gets the same through `theme: 'base'` and `themeVariables` read from the
tokens at render time (primary = brand tint, lines = ink-faint, notes =
surface-sunk, font = Manrope).

### 2.8 Charts

One horizontal bar grammar for `StorageScale`, the pricing provider chart and the
compression-results rows:

- Bars are filled, 4 px corners, 16–20 px tall, on a `--surface-sunk` well.
  No outlined bars.
- Colour by role (2.2): `--neutral-data` = without DeltaGlider,
  `--brand` = with DeltaGlider, `--accent` = reference.
- Values in mono to the right of the bar end, never inside a chip on the bar.
- One legend, top left, sentence case, colour squares at `--r-sm`.
- No gridlines. One baseline. Totals row below a `--rule`, numerals in the stat
  style.
- Before → after pairs in tables: `~~before~~ → after` in mono, with the
  percentage as plain text in `--brand`, not a pill inside a pill.

### 2.9 Motion

- Nothing moves that the visitor did not start. Remove the orbit; the hero
  walkthrough does not autoplay.
- One exception is allowed per page: a single entrance of the message graphic
  on first view (the bars of `StorageScale` growing once, 600 ms, ease-out).
- Hover: colour and border change only, 150 ms. No lift, no scale, no glow.
- `prefers-reduced-motion` stops all of it (already global; keep).

### 2.10 Dark and light parity

- Both themes get the same design, not a tinted copy. Check every change in both.
- Code and ciphertext panels stay dark in both themes (existing rule, keep).
- Screenshots of the admin UI are light in both themes (they are real UI). Frame
  them with `--r-md`, 1 px `--rule`, `--shadow-md`.
- The home hero diagram and every chart use tokens only, so they flip for free.

### 2.11 One message per page

| Page | What a visitor must understand in 5 seconds | The one graphic that carries it |
|---|---|---|
| Home | DeltaGlider is one proxy in front of any S3 bucket; it adds compression, encryption, access control and replication; the clients do not change. | **Hero control-plane diagram** (new, static): clients → proxy (four capability rows inside it) → backends (AWS, Hetzner, Wasabi, MinIO). `StorageScale` is the proof one screen down. |
| `/saas` | Ten builds cost about the storage of one. | **Compact `StorageScale`** in the hero (v1 reference + deltas vs ten full copies). |
| `/regulated` | The key stays in your runtime; the provider stores only ciphertext. | **Runtime → provider diagram** (existing, fixed). |
| `/pricing` | Free up to 15 TB; here is your saving. | **Calculator result** ("You'd save about $15k/year"). The provider chart supports it. |
| `/trial` | Thirty days of direct engineering support, no card, no auto-billing. | **The day-by-day timeline** (fixed and moved up to follow the hero). |
| Case study | 1.71 TB became 134 GB, verified byte for byte. | **The stat card** (existing). |
| Docs | Find the page you need. | No graphic; the search box and the sidebar. |

---

## 3. Change list

Priority: **P0** = defect or strongest incoherence, **P1** = graphics and focal
points, **P2** = polish. Effort: **S** < 1 h, **M** 1–3 h, **L** > 3 h. Each line
is one `style(site): …` commit unless noted. Order inside a priority is the order
of work: tokens, then components, then pages.

### P0

1. **Tokens** (`theme.css`): add `--surface-sunk`, `--neutral-data`, `--ok`,
   `--neg`, `--warn` with light values; `--r-sm/md/pill`; the type scale and
   spacing scale as custom properties. Nothing consumes them yet. — S
2. **Buttons** (`global.css`): make `.btn-primary` the brand fill (same as
   `.btn-brand`, which becomes an alias), secondary = outlined, ghost = text.
   Hover = colour only. Fixes the primary CTA on `/saas`, `/trial`,
   `/regulated`, `/pricing`, the case study. — S
3. **Light-mode leaks**: regulated sidebar background → `--surface-sunk`;
   `.enc-key-chip` → brand chip; `DropIn` minus line → `--neg`; trial `×` →
   `--neg`; calculator amber note → `--warn`; index terminal gradient → panel
   tokens. — S
4. **Emoji out**: `CustomerSegmentBlock` (five emoji → one neutral bullet or a
   glyph per row), `WorkloadFitTable` (✅/❌ → `check`/`cross` glyphs + text),
   `⚿`, `▤` on `/regulated`. Adds `key`, `file`, `check`, `cross` glyphs. — S
5. **Home hero frame defect**: remove `.hero-plate-fade` over the walkthrough
   frame (the dark band). Align the hero to the 1100 px shell. — S
6. **Trial timeline**: move the line into the gutter so it does not cross the
   labels; solid `--rule-strong` line with brand dots, no gradient. — S

### P1

7. **Home hero**: remove `HeroOrbit`. Replace `HeroWalkthrough` in the hero
   with a static `HeroDiagram.astro` (inline SVG, grammar 2.7, horizontal on
   desktop, stacked below 900 px so phones get the graphic too). Only words
   already on the page (S3 API; AWS, Hetzner, Wasabi, MinIO; compression,
   encryption, IAM and SSO, replication). Drop the H1 gradient. — M.
   **Owner decision:** delete `HeroWalkthrough.tsx` (the 90 s video already
   tells the product story one screen down), or keep it below the fold and
   restyle it onto the tokens and Manrope (L: 1 420 lines of inline styles).
   Recommendation: delete; it then also removes Spectral and Hanken Grotesk.
8. **Glyph set**: one colour, one tile (2.6); update every tile user
   (`DropIn`, `ControlPlane`, trial, docs index). — S
9. **Non-sequence numbering out**: `ControlPlane` 01–06 and the home FAQ
   numerals. Widen the `ControlPlane` grid to the shell. — S
10. **One bar chart**: restyle `StorageScale` to filled bars (2.8); restyle the
    pricing provider chart to the same grammar with values outside the bars. — M
11. **`/saas` hero**: replace the stacked-card graphic with a compact
    `StorageScale` (a `compact` prop on the same component). — M
12. **Results table** (`CompressionResultsTable`): one-level before → after
    cells, plain-text percentage, status chips from the system; remove the empty
    top padding of the `/saas` wrapper. — S
13. **Fonts**: remove Spectral and Hanken Grotesk (after item 7), and
    Schibsted Grotesk and Spline Sans Mono from `DocsLayout` / `docs.css`. — S
14. **Mermaid**: `theme: 'base'` with `themeVariables` read from the CSS
    tokens; re-render on theme flip (already wired). — S
15. **Code panels**: one anatomy (filename header, no traffic lights) for the
    home terminal and the drop-in panel. — S

### P2

16. **Eyebrows**: sentence case, 8 px to the H2, remove the ones that restate
    the H2 (home: "See video", "Drawn to scale", "Workload fit", "Get started";
    `/saas`: "The mechanism", "The cost", "Playbook"; similar on other pages).
    No copy other than the removed labels changes. — S
17. **Section rhythm**: one section padding; fix `/saas` "The cost" collision
    and the empty band before the footers of `/saas` and `/trial`. — S
18. **Card anatomy**: testimonial chip inline (not full width); plan rows with
    body-face prices that do not wrap and a brand border for the recommended
    plan instead of a top stripe; playbook as a five-step row or list, smaller
    numerals. — M
19. **Chrome**: footer to one rule and a flat surface; hero washes to one brand
    tint, no fuchsia; header "Beshu DeltaGlider" in one weight. — S
20. **Docs**: remove the grid background; line-clamp card descriptions; the
    docs header on the docs shell; sidebar footer in the body face. — S
21. **`/trial` hero**: remove the dashboard screenshot and its mask; the
    timeline follows the hero. — S
22. **Demo video**: remove the "90S VIDEO" caps label and the outer glow. — S
23. **Source guard** (test): no hex / `rgb(` literals in `marketing/src`
    outside `theme.css`, and no emoji in `.astro` / `.tsx` markup. Fix the class,
    not the site. — S

### Not in this pass

- Copy and numbers (including the two metric formats in the case study).
- The admin UI screenshots themselves.
- Page structure and section order, except moving the trial timeline up
  (item 21), which removes a graphic rather than adding content.

### Verification for every item

Before and after screenshots at 1440 px Chromium and 393 px WebKit, dark and
light; `cd marketing && npm run typecheck && npm test && npm run build`;
`./scripts/check-docs-registry.sh`.
