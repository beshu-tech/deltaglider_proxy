---
name: marketing_frontend_hygiene_apr_2026
description: Code hygiene review of marketing site (React/Tailwind SSG under marketing/). Top DRY issues, shared components discovered, and structural patterns.
type: project
---

Marketing site reviewed 2026-04-27. ~38 files, 9 pages, React 19 + Tailwind v4 + vite-react-ssg.

**Key patterns:**
- `ChecklistGrid` component exists and is used by Regulated + MinioMigration pages, but About.tsx still inlines the same pattern.
- Header.tsx duplicates nav links for desktop + mobile (same 5 links + GitHub written twice).
- Page path list maintained in 3 places: `seo/pages.ts`, `gen-sitemap.mjs`, `check-seo.mjs`.
- `SITE_URL` duplicated in `seo/schema.ts` and `gen-sitemap.mjs`.
- Landing.tsx is the longest file (537 LOC) with a self-contained `UseCaseCarousel` component inline.
- S3Migration.tsx and MultiCloud.tsx both have custom dark hero sections outside the shared `Hero` component — intentional divergence.
- `FeatureCard`, `Section`, `Hero`, `MailtoCTA`, `ScreenshotFrame`, `SEO`, `JsonLd` are well-factored shared components.

**Why:** Tracks marketing frontend hygiene state so future reviews can check progress on these items.
**How to apply:** Reference when asked about marketing frontend cleanup, DRY violations, or component inventory.
