# Site rethink and rewrite — November 2026

Scope: `marketing/` (deltaglider.com). Goal: the site makes an engineer run the
proxy on their own data, and makes a buyer trust the company that sells it.
Constraint from the owner: the site must not look AI-generated, overdone,
sleazy or VC-backed.

Builds on `site-design-audit-2026-09.md` (the visual system). This plan covers
what that audit does not: the message, the page set, the copy and the order of
work.

## 1. Diagnosis

Measured on 2026-11-10: about 78 unique GitHub visitors in 14 days, 3 stars,
about 6 downloads per release. Analytics (Umami) went live the same day, so the
site has no funnel data yet.

1. **The site leads with a category, not a result.** "The object storage
   control plane" names a category that nobody searches for. The measurable
   claim sits below it.
2. **The first step that the site asks for is an email to sales.** The command
   that starts the product is at the bottom of the home page.
3. **The page set is split by buyer type** (`/saas`, `/regulated`). A visitor
   must classify themselves before they learn what the product does. "SaaS" as
   a nav label also reads as "this is a hosted service", which is false.
4. **Search finds nothing.** There are no pages for the problems people type:
   build-artifact storage cost, a MinIO replacement, S3 with SSO.
5. **The only customer quotes are about another product** (ReadonlyREST). The
   section says so, which is honest, but four quotes about another product on
   the home page read as borrowed credibility.
6. **Proof is one case study, and it is our own.**

What is already right, and stays: the proportional storage chart, the
"What we don't claim" section, the workload-fit table that says where the
product does not help, the public pricing, the config diff, the real
screenshots, the migration log with its pitfalls. The copy has no hype words
today (a search for the usual list found none). These are the assets of an
honest site. The rewrite moves them up; it does not replace them.

## 2. Position

One sentence, used everywhere:

> DeltaGlider is an S3 proxy that stores versioned binaries in 60–95% less
> space, in front of the bucket you already have.

Then, second: the same binary gives encryption with your own key, SSO and
per-user IAM, audit, and replication between providers.

The compression claim leads because it is the only one a visitor can verify in
five minutes on their own data, and the only one with a number. The control
plane is the reason to keep the product, not the reason to try it.

Audience, in order:
1. The engineer who owns a bucket of builds, backups or releases and its bill.
2. The platform lead who wants one access model over several providers.
3. The buyer in a regulated company who needs key custody.

## 3. Rules of voice and look

These rules are the answer to "not AI-generated, not overdone, not sleazy, not
VC-backed". Each one is checkable.

### Copy

- **Every number has a source next to it**: the data set, the method, the date,
  a link. A number without a source is removed.
- **Say where the product does not work**, on the same page as the claim:
  already-compressed media, unrelated files, single versions.
- **Say what it costs**: latency of a delta GET, CPU, the single-instance
  limits, the BUSL licence and the 15 TB grant. In plain words, on the page
  where the reader decides.
- **Write as the engineers who run it.** First person plural, past tense for
  what happened ("we moved 145 versions"), present for what the product does.
- **Banned words** (guard: a vitest over `src/pages` and `src/components`):
  seamless, effortless, powerful, robust, blazing, unlock, supercharge,
  empower, leverage, revolutionary, next-generation, enterprise-grade,
  world-class, cutting-edge, game-changing, "at scale", "in minutes" without a
  measured time, "trusted by".
- **Banned shapes**: a list of exactly three adjectives; a headline that is a
  question the next line answers; "It's not X, it's Y"; a closing line that
  repeats the section; an exclamation mark; an em-dash as a pause.
- **One call to action per section**, named for what happens: "Copy the
  command", "Read the migration log", "Email us". Never "Get started",
  "Learn more", "Book a demo".
- **No urgency and no capture**: no pop-up, no chat widget, no newsletter
  modal, no countdown, no gated PDF, no "limited" anything, no cookie banner
  (the analytics is cookieless).
- **People have names.** The case study carries its author and date. The
  footer names the company, its country and a real address for email.

### Look

- **Finish the September audit first** (section 3 of that file): one sans and
  one mono typeface, one brand hue, solid headline, no glows, no orbit, no
  emoji, no decorative numbering, no tracked-caps eyebrows.
- **No decoration that carries no fact.** A graphic stays only when it shows
  data or the real product. No abstract 3D shapes, gradient meshes, glass
  cards, floating blobs or stock illustration.
- **Not the default template.** The pattern "hero, logo wall, three icon
  cards, testimonial carousel, big gradient call to action" is the look of a
  generated site. The home page uses none of these five blocks.
- **Motion only on a user action.** Nothing moves by itself.
- **Real artefacts first**: terminal output copied from a real run,
  screenshots of the real admin UI, the real config diff, the real chart from
  the case-study data.
- **A small set of drawings** (owner decision, 2026-11-10) for the claims that
  text delivers slowly: the read path, the write path, key custody. One
  grammar, in `marketing/src/components/drawings/`: hand-written SVG line
  art, one 2 px stroke, one file shape in every scene, the brand colour only
  for the thing the drawing is about, real file names, no text inside the
  SVG, and a caption that states the claim. No drawing for IAM, SSO, audit
  or replication: the screenshots show those.
- **Text-first layout.** A measure of 65–75 characters, left-aligned, with
  tables for comparisons. References for the tone: sqlite.org, litestream.io,
  restic.net, the Tailscale docs. Plain, dense, dated, signed.

## 4. Page set

| Today | After | Why |
|---|---|---|
| `/` | `/` | Rewritten (section 5). |
| `/saas` | `/build-artifacts` | Named for the problem, not the buyer. 301 from `/saas`. |
| `/regulated` | `/encryption` | Same reason. 301 from `/regulated`. |
| — | `/how-it-works` | One page: PUT, GET, the reference, the integrity check, the costs. Today this is spread over the home page, `/saas` and the docs. |
| — | `/will-it-work` | The fit table, plus one command that estimates the savings on a sample of the reader's bucket. Needs an estimator in the CLI; without one, the page gives the manual steps. |
| `/pricing` | `/pricing` | Keep. The calculator also appears on the home page. |
| `/trial` | `/support` | The page sells 30 days of engineer time, not software. The name says so. 301 from `/trial`. |
| `/case-studies/*` | same | Add an author, a date, and a second study when there is one. |
| — | `/compare/*` | Section 6. |
| — | `/changelog` | The repo CHANGELOG rendered on the site. A dated list of fixes is the strongest "this is maintained" signal we have. |
| `/docs/*` | same | No change. |

Header: **How it works · Use cases · Pricing · Docs · GitHub**. "Use cases"
opens the two pages above and the compare pages. Support moves to the footer
and to the end of each page.

## 5. Page by page

### Home

Order, top to bottom. Each block answers the question the block above raises.

1. **Hero** (approved 2026-11-10): the result as the headline, one sentence of
   mechanism, the `docker run` command with a copy button, the tutorial as the
   primary link, the calculator as the second.
2. **The proof**: the ten-release storage chart, with the source of its data.
3. **Will it work on your data?** The fit table, both columns, with a link to
   `/will-it-work`.
4. **What it changes for your clients**: the one-line config diff.
5. **What it costs**: three measured facts (delta GET latency, CPU per PUT,
   the single-instance contract), each with a link to the detail.
6. **What else is in the binary**: encryption, SSO and IAM, audit,
   replication, lifecycle. A plain two-column list with a screenshot, not six
   numbered cards.
7. **In production**: the ReadonlyREST migration in four numbers and a link.
8. **The questions you would ask**: who makes it, the licence, what happens if
   the company stops, how the code is written (the Claude sentence lives here).
9. **Price in one line**: free to 15 TB, then one flat annual plan, with a
   link.

Removed from the home page: the testimonials about ReadonlyREST (they move to
a short "About the team" block on `/support`, where the reader is evaluating
the company), and the second copy of the quickstart. The 90-second video moves
below the proof.

### `/build-artifacts`

Keep the body of `/saas`. Change the frame: start with the bill (GB stored per
release × releases kept), then the measured result, then the speed table, then
the migration steps. Add CI snippets for GitHub Actions and GitLab that change
only the endpoint.

### `/encryption`

Keep the body of `/regulated`, including "What we don't claim". Start with the
threat ("the provider can read your objects") and the diagram. Add the limits
that the code enforces: a backend without atomic writes makes an encrypted
bucket read-only.

### `/pricing`

Keep. Fix the plan rows that wrap, and state the BUSL terms in two sentences
above the plans: what is free, when it converts to an open-source licence.

### `/support` (was `/trial`)

Shorter. What you get, what it costs after 30 days, a form with three fields
(email, stored TB, backend) with the mailto as the fallback, and the team
block with the ReadonlyREST quotes.

## 6. Pages for search

One page per week, each written from something we measured or ran. No page is
written to hit a keyword with nothing behind it.

1. `/build-artifacts` (above): "reduce S3 storage cost for build artifacts".
2. `/compare/s3-lifecycle-rules`: deleting old versions against keeping all
   of them as deltas. With the arithmetic.
3. `/compare/minio`: DeltaGlider in front of MinIO, and a move away from it.
   States plainly that DeltaGlider is not an object store.
4. `/compare/restic-borg`: backup tools with dedup against a transparent S3
   proxy. Says when restic is the better choice.
5. `/guides/s3-sso`: per-user S3 access with OIDC on any backend.
6. `/guides/backblaze-b2-replica`: a cheap second copy, with the real limit
   (B2 has no conditional writes) and how the proxy handles it.
7. An S3 compatibility page: the verbs supported, the ones not supported, and
   the test suite run against each release.

Rule for every compare page: it has a section "When the other tool is the
better choice". A compare page without it is an advertisement.

## 7. Order of work

| Step | Work | Size | Depends on |
|---|---|---|---|
| 0 | Analytics and funnel events | done | — |
| 1 | Hero rewrite | done | — |
| 2 | Visual system from the September audit | done before this plan (checked 2026-11-10: one typeface pair, no emoji, no orbit, no caps eyebrows) | — |
| 3 | Copy guard as a vitest (`marketing/src/lib/copyGuard.test.ts`) | done | — |
| 4 | Home page reorder and rewrite (section 5) | done | 1, 2 |
| 5 | Rename `/saas`, `/regulated`, `/trial`; header; redirects | done | 4 |
| 6 | `/how-it-works` | done | 5 |
| 6b | `/changelog` | open | 5 |
| 7 | `/support` mail with a pre-filled body (owner chose mailto over a form) | done | 5 |
| 7b | Drawing set: read path, write path, key custody; new social image | done | — |
| 8 | `/will-it-work`, with the estimator if the product gets one | M | product |
| 9 | Search pages, one per week | continuous | 5 |
| 10 | Launch post with measured numbers | S | 4–8 |

Steps 1–5 are one week of work. Each step ships alone and is judged a week
later in Umami by one number: the share of home-page visitors who copy the
command or open the tutorial.

## 8. Decisions for the owner

1. **Renames**: `/saas` → `/build-artifacts`, `/regulated` → `/encryption`,
   `/trial` → `/support`. Yes or no for each.
2. **Testimonials**: move the ReadonlyREST quotes off the home page.
3. **The estimator**: a `deltaglider_proxy estimate <bucket>` command is
   product work, not site work. Without it, step 8 is a manual recipe.
4. **Second case study**: ask the external reporter (ton77v) if their
   deployment can be written up, with or without the company name.
5. **The Claude sentence**: it moves from the hero to "The questions you would
   ask". It stays on the site.
