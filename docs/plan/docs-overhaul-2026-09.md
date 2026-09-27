# Product docs overhaul plan (2026-09)

This plan answers the owner request to bring the documentation up to date, to run the prose through [humanizer](https://github.com/blader/humanizer), to make the docs much better, and to fill them with screenshots. It covers the product docs under `docs/product/`, the screenshots under `docs/screenshots/`, and the two surfaces that render them: the in-product docs viewer and the marketing website.

Every statement about the current state names the file that shows it. The line numbers refer to the tree at commit `2a20365a` on the branch `docs-refresh/work`.

## Summary

The docs follow Diátaxis well at the level of the file tree, but four problems hold them back.

1. Some pages were stale before this change set. The clearest case was `docs/product/how-to/upgrade.md`, which pulled `beshultd/deltaglider_proxy:0.8.x` and said that the config DB "is on schema v6", while 2.0 moves the schema from v24 to v29. The same change set that adds this plan fixes that page and adds `docs/product/how-to/upgrade-to-2-0.md`, but the page still carries old sections (see "Stale how-to content").
2. Some pages mix quadrants, and the largest reference page, `docs/product/reference/configuration.md` (1238 lines), repeats material that other reference pages own.
3. Several features that users of the 2.0 changes need have no how-to at all: the setup wizard, the sessions screen, the compression health screen, the object browser, and the spool directory.
4. The docs have 25 distinct screenshots, all in one theme, most of them taken in June 2026 before a series of UI changes (`git log -- docs/screenshots`, newest commit `df9c2fcf` on 2026-06-15). No script can reproduce them, and no check fails when a referenced image is missing.

The plan fixes these in six phases. It adds about 50 screenshots in both a light and a dark variant, produced by one Playwright spec against a proxy that is seeded with the fixed example cast.

## 1. Assessment per quadrant

### Page inventory

The sizes come from `wc -l docs/product/**/*.md`. The generated `docs/product/changelog.md` (3996 lines) is out of scope.

| Quadrant | Pages | Largest pages |
|---|---|---|
| Tutorials (`tutorials/`) | 3 | `kubernetes-hello-world.md` (169), `secure-your-proxy.md` (177), `first-delta-savings.md` (164) |
| How-to (`how-to/`) | 32, including the new `upgrade-to-2-0.md` | `monitor-with-prometheus.md` (245), `troubleshooting.md` (229), `upgrade.md` (229), `scale-out-with-the-kubernetes-operator.md` (206) |
| Reference (`reference/`) | 15 | `configuration.md` (1238), `admin-api.md` (301), `metrics.md` (207) |
| Explanation (`explanation/`) | 9 | `delta-compression.md` (83), `versioning-vs-s3-versioning.md` (72) |
| Other | `README.md` (89), `faq.md` (52), `manifest.json` | |

### Tutorials

There are three tutorials: `tutorials/first-delta-savings.md`, `tutorials/secure-your-proxy.md` and `tutorials/kubernetes-hello-world.md`. The first two form a good chain, because the second one starts where the first one ends (`secure-your-proxy.md`, first paragraph). The third one is independent.

The gap is that no tutorial teaches the features that make the product a control plane: several backends, replication, lifecycle, and the Jobs screen. A new user meets these only in how-to pages, which assume that the user already knows what to do. The plan adds three tutorials that continue the same chain and use the same cast.

| New tutorial | What the learner builds | Why it is needed |
|---|---|---|
| `tutorials/route-buckets-across-backends.md` | Adds `local-disk` (filesystem) and a MinIO container as `hetzner-fsn1`, routes `releases` and `downloads` to them, and sees both in the Backends and Buckets screens. | The multi-backend model is the main product claim (`README.md`, first paragraph), but only `how-to/route-a-bucket-to-a-backend.md` and `explanation/multi-backend-architecture.md` cover it. |
| `tutorials/protect-releases-with-replication.md` | Replicates `releases` to `aws-dr` (a second MinIO bucket), watches the run in Jobs, and adds a `retain-newest` lifecycle rule on `downloads` with a preview. | Replication, lifecycle and the Jobs screen have only how-to and reference pages. `retain-newest` appears only in `reference/lifecycle.md` (section "Count-based retention"). |
| `tutorials/manage-access-as-code.md` | Exports the IAM state of the tutorial proxy, adds `backup-bot` and the `Engineering` group in YAML, previews the diff, and applies it. | `how-to/manage-iam-as-code.md` assumes a working IAM setup. A lesson that starts from the end state of `secure-your-proxy.md` makes the GitOps path learnable. |

`tutorials/kubernetes-hello-world.md` uses none of the cast names (count of cast names: 0). Its bucket and user names must change to `releases` and `ci-uploader`.

### How-to guides

The how-to set is broad and goal-named. The problems are gaps, mixed pages, one stale page, and UI paths that do not match the admin navigation.

#### Missing how-to pages

These features are in the UI or in `CHANGELOG.md` "## Unreleased", but no how-to page tells the user how to use them.

| Proposed page | Evidence that the feature exists | Current coverage |
|---|---|---|
| `how-to/use-the-setup-wizard.md` | `/_/admin/setup` resolves in `demo/s3-browser/ui/src/adminPathRemap.ts` (`if (path === 'setup') return 'setup'`), component `SetupWizard.tsx`. | No page mentions the wizard (`grep 'admin/setup' docs/product` has no match). |
| `how-to/browse-share-and-bulk-edit-objects.md` | `ObjectTable`, `BulkActionBar`, `DestinationPickerModal`, `InspectorPanel` (share duration), `FilePreview`; CHANGELOG entry "Bulk copy, move, delete and ZIP work for users without admin rights" and "The bulk ZIP download streams, with no size limit". | No page. The browser appears only in the first tutorial. |
| `how-to/manage-admin-sessions.md` | Leaf `access/sessions` in `demo/s3-browser/ui/src/components/adminNavigation.tsx:144`, component `SessionsPanel.tsx`. | Only `reference/admin-api.md` lists the endpoints. |
| `how-to/check-compression-health.md` | Leaf `diagnostics/delta-efficiency` labelled "Compression health" (`adminNavigation.tsx:79-80`), component `DeltaEfficiencyPanel.tsx`. | Only `explanation/the-compute-tax.md` and `reference/admin-api.md` mention it. |
| `how-to/keep-the-newest-versions.md` | `retain-newest` in `reference/lifecycle.md`. | Reference only. `how-to/expire-and-archive-objects.md` covers age-based rules. |
| `how-to/size-the-spool-directory.md` | CHANGELOG "Delta objects larger than 16 MiB use the spool by default" and "One multipart upload can hold at most half of the spool budget"; step 2 of `how-to/upgrade-to-2-0.md`. | Only `reference/configuration.md`, `reference/cli.md` and the upgrade page. |
| `how-to/run-behind-a-reverse-proxy.md` | CHANGELOG "`DGP_TRUST_PROXY_HEADERS=true` needs `DGP_TRUSTED_PROXY_CIDRS`", which stops 1.19 setups from booting. | Spread over `serve-tls.md`, `rate-limits.md`, `go-to-production.md` and `troubleshooting.md`, with no single recipe. |
| `how-to/copy-and-verify-data-with-the-s3-cli.md` | The `s3` command family in `reference/cli.md` (section "`s3` — client command family"), CHANGELOG "The `s3` verbs stream large objects". | Reference only. |
| `how-to/stamp-metadata-on-existing-objects.md` | The backfill job (`reference/jobs.md`, section "Metadata backfill"; `JobsPanel.tsx:402`). | One step inside `how-to/migrate-existing-data-into-the-proxy.md`. |

#### Mixed-quadrant how-to pages

| Page | What is mixed in | Fix |
|---|---|---|
| `how-to/backend-capability-validation.md` | "What the proxy validates, and why" and "How the probe works" are explanation; "The messages, verbatim, and their fixes" is reference. The title carries a parenthetical. | Move the "why" to a new `explanation/conditional-writes-and-backends.md`, move the message catalog to `how-to/troubleshooting.md`, and rename the rest to `how-to/use-a-non-cas-backend-as-a-replication-target.md` (with a manifest redirect). |
| `how-to/scale-out-with-the-kubernetes-operator.md` | "Why you can't just set `replicas: 3` on a plain Deployment" (line 12) and "Know the trade-offs" (line 173) are explanation. | Move both into the new explanation page on multi-instance consistency (see the explanation section). |
| `how-to/diagnose-backend-connectivity.md` | "What happens while a backend is down" and "What happens when a backend hangs" describe behaviour, not steps. | Keep one sentence each and link to the new explanation page. |
| `how-to/view-live-logs.md` | "What it is — and isn't" (line 29). | Fold the facts into `reference/configuration.md` (log ring), keep the steps. |

#### Stale how-to content

- `how-to/upgrade.md` showed `0.8.x` image tags and schema v6 until this change set, which updates those facts. It still carries the TOML to YAML conversion and the v0.9 encryption change. The fix is in the merge section below.
- UI paths in bold do not match the admin navigation in `demo/s3-browser/ui/src/components/adminNavigation.tsx`. "Settings → Jobs" appears in `encrypt-data-at-rest.md`, `expire-and-archive-objects.md`, `move-a-bucket-between-backends.md`, `replicate-a-bucket.md` and `rotate-encryption-keys.md`, but Jobs is a leaf of the Storage group (lines 153 and 172). "Settings → Observability → Audit" appears in `create-iam-users.md`, `publish-a-public-folder.md` and `restrict-access-with-conditions.md`, but the label is "Audit log" (line 86). The plan adds a check that every bold UI path matches a group and a label in `ADMIN_IA` (phase 0).
- `how-to/troubleshooting.md` names a Coolify host (around line 81). That is a production detail of one deployment, and it belongs in `serve-tls.md` as one example among others, or nowhere.

#### Overlaps and merges

| Pages | Overlap | Decision |
|---|---|---|
| `how-to/upgrade.md` and `how-to/upgrade-to-2-0.md` | Both give the backup, swap and verify routine. `upgrade-to-2-0.md` already links to `upgrade.md` for it (first paragraph). | Keep both. Make `upgrade.md` version-neutral: the routine, the rule that schema migrations are one-way, and an index of version-specific guides. Move the TOML and v0.9 sections into a short "Upgrades from 1.x and older" section at the end, or delete them and link to the changelog, because a v1.4 install must first pass through 1.19. |
| `tutorials/kubernetes-hello-world.md`, `how-to/deploy-on-kubernetes.md`, `how-to/scale-out-with-the-kubernetes-operator.md` | All three install the proxy on Kubernetes. The Helm page already sends multi-pod users to the operator (section "Replicas", line 148). | Keep three pages, because they are three quadrant roles (lesson, single-pod recipe, multi-pod recipe). Add the same three-row "which page do I need" table at the top of both how-to pages, and move the operator's "why" sections to explanation. |
| `reference/encryption.md` and `explanation/encryption-at-rest.md` | Both describe the four modes. The reference has the modes table (section "Modes"); the explanation has "The modes, and the real difference between them". | Keep the table only in the reference. The explanation keeps the threat model and the reasons, and links to the table. |
| `how-to/run-multiple-instances.md` and `explanation/multi-backend-architecture.md` (section "HA and the config-sync trade-off") | Both explain what is shared between instances. | Move the reasons into the new explanation page; keep the steps in the how-to. |

#### Pages with no example from the fixed cast

The count of cast names (`hetzner-fsn1`, `local-disk`, `aws-dr`, `releases`, `db-archive`, `downloads`, `ci-uploader`, `backup-bot`, `dana`, `Engineering`) is 0 in these pages: `faq.md`, `how-to/deploy-on-kubernetes.md`, `how-to/monitor-with-prometheus.md`, `how-to/view-live-logs.md`, `reference/admin-api.md`, `reference/capacity-planning.md`, `reference/metrics.md`, `reference/s3-api-compatibility.md`, `tutorials/kubernetes-hello-world.md`. The FAQ and the S3 compatibility list need no examples. The others need at least one command or YAML block that uses the cast.

These pages invent names outside the cast: `acme-firmware` in `how-to/migrate-existing-data-into-the-proxy.md` (5 times), `acme-db` in `how-to/route-a-bucket-to-a-backend.md` and `reference/configuration.md`, `acme-proxy` and `acme-bootstrap` in `how-to/create-iam-users.md`, `acme-admin` and `acme-rocks` in `tutorials/secure-your-proxy.md`, `acme-prod` in `reference/configuration.md` and `explanation/multi-backend-architecture.md`, and `foo` in `how-to/troubleshooting.md` and `reference/encryption.md`. Upstream real-bucket names for an `alias` (such as `acme-db-archive-prod`) are a legitimate exception, because the cast has no upstream names; the plan proposes to add one upstream name to the cast in the repo `CLAUDE.md` so that every page uses the same one.

### Reference

#### `reference/configuration.md` must be split

The page is 1238 lines and has 33 second-level sections (`grep '^##'`). It repeats material that other reference pages own:

| Section in `configuration.md` | Line | Owning page |
|---|---|---|
| "Lifecycle rules" | 807 | `reference/lifecycle.md` |
| "Event delivery", "Slack format" | 853, 888 | `reference/event-outbox.md` |
| "Encryption at rest" | 933 | `reference/encryption.md` |
| "CLI subcommands" | 987 | `reference/cli.md` |
| "Rate limiting" | 664 | `reference/rate-limits.md` |
| "Admission chain" | 528 | nothing yet; see below |

The proposed split, with the manifest groups unchanged:

| New page | Content |
|---|---|
| `reference/configuration.md` | File layout, shorthands, search order, what the admin UI does to the file on save, `${env:NAME}` expansion, and a table of contents that links to the pages below. About 250 lines. |
| `reference/configuration-server.md` | "Server / Advanced", "Delta engine", "TLS", the spool settings. |
| `reference/configuration-storage.md` | "Storage backend", "Multi-backend routing", "Bucket policies", "Job leases", "Config sync". |
| `reference/configuration-access.md` | "Access — authentication", "Access — IAM mode", "Security", "Admission chain". |
| `reference/environment-variables.md` | "Environment variable registry" (line 1096 to the end). This is the table that operators search most. |
| Deleted from the page | The duplicated lifecycle, event delivery, encryption and CLI sections, which become one-line links. |

The "Full example" (line 993) stays on the overview page, because it is the one place that shows the whole document.

#### Other reference problems

- `reference/admin-api.md` holds "Keyboard shortcuts (app-wide)" (line 265) and "Resource limits (env vars)" (line 251). Neither is part of the admin API. The shortcuts move to a new `reference/admin-ui.md` (below); the limits move to `reference/environment-variables.md`.
- `reference/lifecycle.md` ends with "Deferred" (line 124), which is a roadmap item. A reference page states what exists; this list belongs in `docs/plan/`.
- `reference/capacity-planning.md` is titled "Capacity planning and hardware sizing" and ends with "Sizing checklist" (line 46), which is a how-to. Rename to `reference/resource-profile.md` (facts) and move the checklist to a new `how-to/size-hardware.md`.
- `reference/metrics.md` opens with a screenshot of the analytics dashboard (line 5). The page lists Prometheus series; the dashboard picture belongs in `how-to/monitor-with-prometheus.md`, which already has it (line 225).

#### Missing reference pages

| Proposed page | Content |
|---|---|
| `reference/admin-ui.md` | Every admin route (`/_/admin/<leaf>` for the 17 leaves in `adminNavigation.tsx`, plus `/_/admin/setup`), what each screen does, the deep links `?job=<id>&tab=<definition|runs|failures|preview|verify>` (`components/jobs/JobDrawer.tsx:422-437`), the keyboard shortcuts, the "from env" badge rule, and one small screenshot per screen. This is the page that the screenshot pipeline keeps honest. |
| `reference/environment-variables.md` | See the split above. |
| `reference/spool.md` or a section in `configuration-server.md` | `DGP_SPOOL_DIR`, `_MAX_BYTES`, `_THRESHOLD_BYTES`, `_ACQUIRE_TIMEOUT_SECS`, `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`, with the 16 MiB default from the 2.0 changelog. |

### Explanation

The nine explanation pages are short and readable. They lack two topics that the how-to pages keep explaining inline.

| Proposed page | Pulls content from |
|---|---|
| `explanation/running-several-instances.md` | What is shared through the sync bucket and what stays per instance, why multipart needs a directory-hash router, the three-way IAM merge, and the leases. Today this is spread over `how-to/run-multiple-instances.md` (section 7), `how-to/scale-out-with-the-kubernetes-operator.md` (line 12 and line 173), `explanation/multi-backend-architecture.md` (section "HA and the config-sync trade-off") and `how-to/deploy-on-kubernetes.md` (section "Replicas"). |
| `explanation/conditional-writes-and-backends.md` | Why a backend must support conditional writes to hold client-writable buckets in a multi-instance setup, and why `replication_target_only` makes Backblaze B2 safe. Source: `how-to/backend-capability-validation.md` (sections "What the proxy validates, and why" and "How the probe works"). |
| `explanation/memory-and-the-spool.md` | Why a PUT holds the body in memory up to `max_object_size`, why a large delta GET decodes to a spool file before the first byte, and what the spool budget protects. The source is the "Streaming codec" decision in the repo `CLAUDE.md` and `docs/plan/streaming-delta-any-size.md`. |

The explanation pages use headings with a lot of attitude ("pointedly what it can't", "The honest costs", "Open mode, honestly", "What compresses well, and what honestly doesn't"). The humanizer pass (below) will flag these as "inflation", and the rewrite should state the limit plainly.

### Prose: the humanizer pass

[humanizer](https://github.com/blader/humanizer) is an agent skill that detects 25 patterns of machine-written prose in five groups: staging instead of stating, rhythm by rule, inflation and borrowed authority, formatting by rule, and leftovers from chat and draft. This change set uses it as a reference: the repository is cloned outside the repo, and its rules are applied by hand, page by page. It is not a dependency of the repo.

The em dash is one of the strongest signals it looks for. Before this change set, the pages with the most em dashes were `reference/configuration.md` (56 lines that contain one), `reference/metrics.md` (34), `how-to/troubleshooting.md` (34), `README.md` (26), `reference/admin-api.md` (25), `tutorials/secure-your-proxy.md` (23) and `how-to/upgrade.md` (22) (`grep -c '—'`). Bold-label lists are the second common signal. This change set removes the em dashes from prose and most decorative bold, so the remaining work is the lint in rule 3.

The pass has three rules, so that humanizer and the repo style do not fight:

1. Run humanizer on one page at a time and read its diff; never accept a rewrite that changes a fact, a number, a flag or a code block.
2. After humanizer, apply the repo prose style from the repo `CLAUDE.md` ("Prose style"): complete sentences with articles, "requests" and not "calls", the mechanism before the consequence, one idea per sentence. Where humanizer shortens a sentence into a fragment, the repo style wins.
3. Add a lint (phase 0) that fails on an em dash in `docs/product/**` outside code blocks, so the pattern does not come back.

## 2. Screenshot plan

### Current state

The docs reference 25 distinct files under `/_/screenshots/` (`grep -rhoE '/_/screenshots/[A-Za-z0-9_.-]+' docs/product`). All 25 exist in `docs/screenshots/`; none is missing. Three files in `docs/screenshots/` are not referenced by any product doc: `advanced_security.jpg` (only the repo root `README.md`), `cmdk-palette.jpg` (nothing) and `analytics-hero.jpg` (only `marketing/src/pages/trial.astro:112`). `demo/s3-browser/ui/src/components/DocsLanding.tsx:93` and `:96` hard-code `filebrowser.jpg` and `analytics.jpg`, and `marketing/src/components/ControlPlane.astro:33-43` hard-codes `iam.jpg`, `object-replication.jpg` and `analytics.jpg`.

| Existing file | Size | Pixels | Referenced by |
|---|---|---|---|
| `admission-rules.jpg` | 180 KB | 2294x1964 | `explanation/security-model.md`, `how-to/gate-requests-with-admission-rules.md` |
| `analytics.jpg` | 882 KB | 4148x2352 | `reference/metrics.md`, `how-to/monitor-with-prometheus.md`, `DocsLanding.tsx` |
| `audit-log.jpg` | 349 KB | 2294x1964 | `how-to/trace-requests.md` |
| `backup-restore.jpg` | 289 KB | 1830x1964 | `how-to/back-up-and-restore.md` |
| `bucket-policies.jpg` | 274 KB | 2294x1964 | `how-to/set-bucket-compression-and-quotas.md`, `how-to/publish-a-public-folder.md` |
| `config-access-form.jpg` | 288 KB | 2294x1964 | `reference/configuration.md` |
| `config-limits-form.jpg` | 215 KB | 1560x1330 | `reference/configuration.md` |
| `config-storage-form.jpg` | 218 KB | 2294x1964 | `reference/configuration.md`, `how-to/route-a-bucket-to-a-backend.md` |
| `data-path-architecture.jpg` | 211 KB | 1408x768 | `explanation/multi-backend-architecture.md` (a diagram, not a screenshot) |
| `declarative-iam-diff.jpg` | 240 KB | 2294x1964 | `how-to/manage-iam-as-code.md` |
| `delta-savings-badge.jpg` | 196 KB | 2294x1964 | `tutorials/first-delta-savings.md`, `explanation/delta-compression.md` |
| `encryption-enable.jpg` | 325 KB | 2294x1964 | `how-to/encrypt-data-at-rest.md` |
| `events-webhook.jpg` | 262 KB | 2294x1964 | `how-to/send-event-notifications.md` |
| `filebrowser.jpg` | 176 KB | 1147x1440 | `README.md`, `DocsLanding.tsx` |
| `iam.jpg` | 252 KB | 2294x1964 | `how-to/create-iam-users.md`, `tutorials/secure-your-proxy.md` |
| `jobs-drawer-runs.jpg` | 152 KB | 2294x1964 | `how-to/replicate-a-bucket.md` |
| `jobs-screen.jpg` | 200 KB | 2294x1964 | `explanation/jobs-and-durability.md`, `how-to/move-a-bucket-between-backends.md`, `how-to/rotate-encryption-keys.md` |
| `lifecycle-preview.jpg` | 201 KB | 2294x1964 | `how-to/expire-and-archive-objects.md` |
| `migrate-job.jpg` | 235 KB | 2294x1964 | `how-to/move-a-bucket-between-backends.md` |
| `oauth_group_mapping.jpg` | 246 KB | 2294x1964 | `how-to/set-up-sso.md` |
| `oauth_login.jpg` | 126 KB | 1147x1440 | `how-to/set-up-sso.md`, `reference/authentication.md` |
| `object-replication.jpg` | 220 KB | 2294x1964 | `how-to/replicate-a-bucket.md` |
| `reencrypt-proposal.jpg` | 265 KB | 2294x1964 | `how-to/encrypt-data-at-rest.md` |
| `request-trace.jpg` | 213 KB | 2294x1964 | `how-to/trace-requests.md`, `how-to/gate-requests-with-admission-rules.md` |
| `storage_backends.jpg` | 252 KB | 2294x1964 | `explanation/multi-backend-architecture.md`, `how-to/route-a-bucket-to-a-backend.md` |

The alt texts are labels, not sentences (for example "Jobs screen" in `how-to/move-a-bucket-between-backends.md:48`). The viewers turn the alt text into a caption (`DocsPage.tsx:406-410` wraps every image in a `Lightbox` with the alt text as the caption), so a full sentence is both better for screen readers and a better caption. The file names mix underscores and hyphens (`storage_backends.jpg` and `jobs-screen.jpg`); new names use hyphens only.

Several screens changed after June 2026, so these existing shots are likely stale: `events-webhook.jpg` (CHANGELOG "per-endpoint delivery state in the event log"), `oauth_group_mapping.jpg` (CHANGELOG "The OIDC provider form sets the network policy"), `request-trace.jpg` (CHANGELOG "The rule tester shows what an anonymous caller may do"), `config-access-form.jpg` (CHANGELOG "The bootstrap access key id is visible, and its removal is explicit"), `config-limits-form.jpg` and `advanced_security.jpg` (the Limits and Security forms moved into the System page; `adminPathRemap.ts` maps `limits` and `security` to `system`). The plan re-shoots all 25 from the pipeline anyway, so nothing depends on this list being complete.

### Routes that the shots use

All routes are checked against the UI source. Views: `/_/browse`, `/_/upload`, `/_/docs`, `/_/admin` (`demo/s3-browser/ui/src/urlState.ts:19-37`). The browser takes `?object=`, `?preview=` and `?q=` (`urlState.ts:114-116`). Admin leaves: `dashboard`, `diagnostics/trace`, `diagnostics/delta-efficiency`, `diagnostics/audit`, `diagnostics/logs`, `access/credentials`, `access/users`, `access/groups`, `access/external-auth`, `access/admission`, `access/sessions`, `storage/backends`, `storage/buckets`, `jobs`, `integrations/event-delivery`, `integrations/event-outbox`, `system` (`adminNavigation.tsx:66-208`), plus `setup` (`adminPathRemap.ts`). Jobs deep links use `?job=<id>&tab=<key>` (`JobsPanel.tsx:286`, tab keys in `JobDrawer.tsx:422-437`), with ids `replication:<rule>`, `lifecycle:<rule>` and `maintenance:<n>`.

### The shot list

Each file is produced twice, as `<name>.light.webp` and `<name>.dark.webp` (see section 3). The "state" column assumes the seeded fixture from section 3 unless it says otherwise. "Re-shoot" marks a replacement for an existing file, which then gets a new name.

| # | Page | Shot | Route | State to set up | Alt text |
|---|---|---|---|---|---|
| 1 | `README.md` | `browser-releases` (re-shoot of `filebrowser.jpg`) | `/_/browse/releases/firmware/widget-3000/` | Seeded `releases` with four firmware versions. | The object browser lists four firmware tarballs in the releases bucket, and every row shows the size that the proxy stored for it. |
| 2 | `README.md` | `dashboard-savings` (re-shoot of `analytics.jpg`) | `/_/admin/dashboard` | Seeded data in all three buckets; usage scan finished. | The dashboard shows how many bytes the delta compression saved in each bucket and what that saving is worth each month. |
| 3 | `tutorials/first-delta-savings.md` | `tutorial-empty-browser` | `/_/browse` | Open access, no buckets. | The object browser of a new proxy shows no buckets and offers a button to create the first one. |
| 4 | `tutorials/first-delta-savings.md` | `tutorial-create-bucket` | `/_/browse` | Create-bucket dialog open with the name `releases` typed. | The create-bucket dialog holds the name releases, ready to be confirmed. |
| 5 | `tutorials/first-delta-savings.md` | `tutorial-upload-first-version` | `/_/upload` | Target `releases/firmware/widget-3000/`, `fw-1.4.0.tar` queued and finished. | The upload page shows that fw-1.4.0.tar finished uploading into the widget-3000 folder of the releases bucket. |
| 6 | `tutorials/first-delta-savings.md`, `explanation/delta-compression.md` | `inspector-delta-savings` (re-shoot of `delta-savings-badge.jpg`) | `/_/browse/releases/firmware/widget-3000/?object=firmware/widget-3000/fw-1.4.1.tar` | Inspector open on the second version. | The object inspector shows that fw-1.4.1.tar is stored as a delta and uses a small fraction of its original size. |
| 7 | `tutorials/secure-your-proxy.md` | `login-bootstrap` | `/_/` | Bootstrap mode, signed out. | The sign-in page asks for the admin password before it opens the proxy. |
| 8 | `tutorials/secure-your-proxy.md` | `credentials-bootstrap` (re-shoot of `config-access-form.jpg`) | `/_/admin/access/credentials` | Bootstrap SigV4 credentials set. | The credentials page shows that the proxy is in bootstrap mode and shows the access key id that S3 clients use. |
| 9 | `tutorials/secure-your-proxy.md`, `how-to/create-iam-users.md` | `users-ci-uploader` (re-shoot of `iam.jpg`) | `/_/admin/access/users` | `ci-uploader` selected, one rule that allows read, write and list on `releases/firmware/*`. | The users page shows the ci-uploader user with one rule that lets it read, write and list only under releases/firmware/. |
| 10 | `tutorials/secure-your-proxy.md` | `audit-access-denied` | `/_/admin/diagnostics/audit` | One refused `GetObject` by `ci-uploader` on `db-archive`, filter set to the user. | The audit log shows that the proxy refused a read of the db-archive bucket by ci-uploader. |
| 11 | `tutorials/route-buckets-across-backends.md` (new) | `backends-three` (re-shoot of `storage_backends.jpg`) | `/_/admin/storage/backends` | Backends `hetzner-fsn1`, `local-disk`, `aws-dr`, all healthy. | The backends page lists hetzner-fsn1, local-disk and aws-dr, and each one has a green health badge. |
| 12 | `tutorials/route-buckets-across-backends.md` (new), `how-to/route-a-bucket-to-a-backend.md` | `buckets-routing` (re-shoot of `config-storage-form.jpg`) | `/_/admin/storage/buckets` | `releases` on `hetzner-fsn1`, `downloads` on `local-disk`, `db-archive` row expanded. | The buckets page shows which backend holds each bucket, with the db-archive row open to change its backend. |
| 13 | `how-to/route-a-bucket-to-a-backend.md` | `backends-add` | `/_/admin/storage/backends` | "Add backend" form open, type S3, name `aws-dr`. | The add-backend form collects the name, the endpoint, the region and the credentials of a new S3 backend. |
| 14 | `tutorials/protect-releases-with-replication.md` (new), `how-to/replicate-a-bucket.md` | `replication-rule-editor` (re-shoot of `object-replication.jpg`) | `/_/admin/jobs` | Rule editor open for `releases` to `releases-dr` on `aws-dr`. | The replication editor copies the releases bucket to the releases-dr bucket on the aws-dr backend. |
| 15 | `tutorials/protect-releases-with-replication.md`, `how-to/replicate-a-bucket.md` | `job-runs` (re-shoot of `jobs-drawer-runs.jpg`) | `/_/admin/jobs?job=replication:releases-to-dr&tab=runs` | One finished run. | The runs tab of the replication job shows one finished run and how many objects it copied. |
| 16 | `how-to/replicate-a-bucket.md` | `job-failures` | `/_/admin/jobs?job=replication:releases-to-dr&tab=failures` | One failure provoked by a key that the destination refuses. | The failures tab lists one object that the replication job could not copy, with the error that the destination returned. |
| 17 | `reference/jobs.md`, `explanation/jobs-and-durability.md` | `jobs-all-kinds` (re-shoot of `jobs-screen.jpg`) | `/_/admin/jobs` | One replication rule, one lifecycle rule, one finished migrate, one finished re-encrypt. | The Jobs page lists replication rules, lifecycle rules and one-off maintenance jobs in one table with their status and progress. |
| 18 | `how-to/expire-and-archive-objects.md` | `lifecycle-rule-editor` | `/_/admin/jobs` | Lifecycle editor open, rule `expire-old-downloads` on `downloads`, 30 days. | The lifecycle editor deletes objects in the downloads bucket that are older than 30 days. |
| 19 | `how-to/expire-and-archive-objects.md`, `how-to/keep-the-newest-versions.md` (new) | `lifecycle-preview` (re-shoot) | `/_/admin/jobs?job=lifecycle:expire-old-downloads&tab=preview` | Seeded objects with old `dg-created-at` through the write path. | The preview tab lists the objects that the lifecycle rule would delete, before anything is deleted. |
| 20 | `how-to/keep-the-newest-versions.md` (new) | `lifecycle-retain-newest` | `/_/admin/jobs` | Rule `keep-five-builds` with `retain-newest: 5` on `releases/firmware/`. | The lifecycle editor keeps the five newest firmware builds and deletes the older ones. |
| 21 | `how-to/move-a-bucket-between-backends.md` | `migrate-dialog` (re-shoot of `migrate-job.jpg`) | `/_/admin/storage/buckets` | "Migrate data…" dialog of `db-archive`, target `hetzner-fsn1`. | The migrate dialog moves the db-archive bucket from local-disk to hetzner-fsn1. |
| 22 | `how-to/move-a-bucket-between-backends.md`, `how-to/troubleshooting.md` | `browser-write-gate-banner` | `/_/browse/db-archive/` | A migrate job held in its copy phase (a large seeded object and a slow backend). | The object browser warns that writes to db-archive are paused while a migrate job runs. |
| 23 | `how-to/stamp-metadata-on-existing-objects.md` (new) | `backfill-dialog` | `/_/admin/jobs` | "Backfill metadata…" modal open for `db-archive`. | The backfill dialog stamps proxy metadata onto the objects in db-archive that were written without the proxy. |
| 24 | `how-to/encrypt-data-at-rest.md` | `backend-encryption` (re-shoot of `encryption-enable.jpg`) | `/_/admin/storage/backends` | `hetzner-fsn1` expanded, mode `aes256-gcm-proxy`, generated key masked. | The backend form turns on proxy-side AES-256-GCM encryption for hetzner-fsn1. |
| 25 | `how-to/encrypt-data-at-rest.md`, `how-to/rotate-encryption-keys.md` | `reencrypt-dialog` (re-shoot of `reencrypt-proposal.jpg`) | `/_/admin/jobs` | "Re-encrypt buckets…" open, `releases` selected. | The re-encrypt dialog proposes to rewrite the objects in releases with the new key. |
| 26 | `how-to/rotate-encryption-keys.md` | `legacy-key-banner` | `/_/admin/storage/backends` | `hetzner-fsn1` key rotated, `legacy_key` set, nothing left to read with it. | The backend page says that the previous key is still kept for reads and offers to clear it because no object needs it. |
| 27 | `how-to/set-bucket-compression-and-quotas.md` | `bucket-quota` (re-shoot of `bucket-policies.jpg`) | `/_/admin/storage/buckets` | `releases` row expanded, quota 50 GB, compression on. | The releases bucket row shows delta compression turned on and a soft quota of 50 GB. |
| 28 | `how-to/check-compression-health.md` (new) | `compression-health` | `/_/admin/diagnostics/delta-efficiency` | Seeded delta and passthrough objects. | The compression health page shows which prefixes compress well and which ones store objects as they are. |
| 29 | `how-to/publish-a-public-folder.md` | `bucket-public-prefix` | `/_/admin/storage/buckets` | `downloads` public access set to "Specific prefixes" with `public/`. | The downloads bucket row makes only the public/ prefix readable without credentials. |
| 30 | `how-to/gate-requests-with-admission-rules.md`, `explanation/security-model.md` | `request-rules` (re-shoot of `admission-rules.jpg`) | `/_/admin/access/admission` | Two blocks: a deny for anonymous writes, a reject during maintenance. | The request rules page lists two rules that run before authentication, in the order that the proxy checks them. |
| 31 | `how-to/trace-requests.md`, `how-to/gate-requests-with-admission-rules.md` | `rule-tester` (re-shoot of `request-trace.jpg`) | `/_/admin/diagnostics/trace` | Anonymous `GET /downloads/public/installer.sh`. | The rule tester shows that an anonymous request for downloads/public/installer.sh is allowed, and which rule allowed it. |
| 32 | `how-to/trace-requests.md` | `rule-tester-anonymous` | `/_/admin/diagnostics/trace` | Anonymous caller, the section that lists what it may do. | The rule tester lists what an anonymous caller may do on each bucket. |
| 33 | `how-to/trace-requests.md` | `audit-log` (re-shoot) | `/_/admin/diagnostics/audit` | About 20 seeded entries from the cast users. | The audit log lists recent admin and S3 actions with the user, the bucket and the source address of each one. |
| 34 | `how-to/view-live-logs.md` | `system-logs` | `/_/admin/diagnostics/logs` | Level filter `warn`, one warning provoked. | The system logs page shows the live log of the proxy filtered to warnings. |
| 35 | `how-to/create-iam-users.md` | `user-permission-editor` | `/_/admin/access/users` | `dana` open in the permission editor. | The permission editor gives dana read and list rights on every bucket. |
| 36 | `how-to/create-iam-users.md` | `group-engineering` | `/_/admin/access/groups` | Group `Engineering` with `dana` and `backup-bot`. | The Engineering group has two members and one shared rule that both inherit. |
| 37 | `how-to/restrict-access-with-conditions.md` | `user-ip-condition` | `/_/admin/access/users` | `backup-bot` rule with a source IP condition `203.0.113.0/24`. | The backup-bot rule applies only to requests from the 203.0.113.0/24 network. |
| 38 | `how-to/set-up-sso.md` | `oidc-provider` | `/_/admin/access/external-auth` | Generic OIDC provider pointing at the stub issuer (section 3), network policy field visible. | The external authentication page configures an OIDC provider, including the network policy for an issuer in a private network. |
| 39 | `how-to/set-up-sso.md` | `group-mapping` (re-shoot of `oauth_group_mapping.jpg`) | `/_/admin/access/external-auth` | One mapping rule from the IdP group `eng` to `Engineering`. | The mapping rule puts every user whose identity provider group is eng into the Engineering group. |
| 40 | `how-to/set-up-sso.md`, `reference/authentication.md` | `login-sso` (re-shoot of `oauth_login.jpg`) | `/_/` | Signed out, one OIDC provider configured. | The sign-in page offers a button to sign in with the configured identity provider next to the password field. |
| 41 | `how-to/manage-iam-as-code.md` | `apply-dialog-iam-diff` (re-shoot of `declarative-iam-diff.jpg`) | `/_/admin/access/users` | Declarative mode, YAML import adds `backup-bot`, apply dialog open. | The apply dialog lists the users and groups that the YAML will create, change or delete before anything changes. |
| 42 | `how-to/manage-iam-as-code.md` | `users-declarative-banner` | `/_/admin/access/users` | Declarative mode. | The users page says that the YAML file owns the IAM state and that the page is read-only. |
| 43 | `how-to/manage-admin-sessions.md` (new) | `sessions` | `/_/admin/access/sessions` | Two sessions: the admin and a browser session of `dana`. | The sessions page lists the admin session and a browser session of dana, and each one can be ended. |
| 44 | `how-to/send-event-notifications.md` | `event-webhook` (re-shoot of `events-webhook.jpg`) | `/_/admin/integrations/event-delivery` | Webhook to a local sink started by the script. | The event delivery page sends object events from the releases bucket to a webhook. |
| 45 | `how-to/send-event-notifications.md` | `event-slack` | `/_/admin/integrations/event-delivery` | Slack card in webhook mode, URL masked. | The Slack card sends object events to a Slack channel through an incoming webhook. |
| 46 | `how-to/send-event-notifications.md`, `reference/event-outbox.md` | `event-log` | `/_/admin/integrations/event-outbox` | Ten delivered events and one that waits for a retry. | The event log shows each object event and whether each endpoint received it. |
| 47 | `how-to/back-up-and-restore.md` | `system-backup` (re-shoot of `backup-restore.jpg`) | `/_/admin/system` | Scrolled to the Backup card. | The Backup card downloads a full backup and restores one from a file. |
| 48 | `how-to/serve-tls.md`, `how-to/go-to-production.md` | `system-listener-tls` | `/_/admin/system` | Listener and TLS card, self-signed TLS on. | The listener card shows the address that the proxy listens on and whether it serves TLS. |
| 49 | `how-to/run-multiple-instances.md` | `system-sync-state` | `/_/admin/system` | `config_sync_bucket` on MinIO, one upload done. | The sync card shows the sync bucket and the time of the last upload and download of the IAM database. |
| 50 | `reference/configuration.md` | `system-limits` (re-shoot of `config-limits-form.jpg`) | `/_/admin/system` | Limits card, `DGP_MAX_OBJECT_SIZE` set in the environment. | The limits card shows the request limits, and a field that an environment variable sets is read-only with a from-env badge. |
| 51 | `how-to/diagnose-backend-connectivity.md` | `backend-unhealthy` | `/_/admin/storage/backends` | `aws-dr` pointed at a closed port. | The backends page shows a red health badge on aws-dr and the error that its probe returned. |
| 52 | `how-to/backend-capability-validation.md` | `backend-non-cas` | `/_/admin/storage/backends` | Debug build with `DGP_TEST_FORCE_NONCAS_BACKEND` (see section 3). | The backends page warns that aws-dr does not support conditional writes and links to the page that explains what to do. |
| 53 | `how-to/use-the-setup-wizard.md` (new) | `setup-wizard-backend` | `/_/admin/setup` | Fresh proxy, step 2 of 5. | The setup wizard asks for the first storage backend. |
| 54 | `how-to/use-the-setup-wizard.md` (new) | `setup-wizard-done` | `/_/admin/setup` | Last step. | The last step of the setup wizard shows the YAML that it wrote and the next things to do. |
| 55 | `how-to/browse-share-and-bulk-edit-objects.md` (new) | `browser-bulk-actions` | `/_/browse/releases/firmware/widget-3000/` | Three objects selected. | Three selected objects show the bulk bar with copy, move, ZIP download and delete. |
| 56 | `how-to/browse-share-and-bulk-edit-objects.md` (new) | `browser-destination-picker` | same | Copy dialog open, destination `downloads/public/`. | The copy dialog picks the public folder of the downloads bucket as the destination. |
| 57 | `how-to/browse-share-and-bulk-edit-objects.md` (new) | `inspector-share-link` | `/_/browse/downloads/public/?object=public/installer.sh` | Share duration 1 day selected. | The inspector makes a download link for installer.sh that expires after one day. |
| 58 | `how-to/browse-share-and-bulk-edit-objects.md` (new) | `browser-preview` | `/_/browse/releases/reports/?preview=reports/build-notes.md` | A small text object. | The preview shows the text of build-notes.md without downloading it. |
| 59 | `reference/admin-ui.md` (new) | `command-palette` (re-shoot of `cmdk-palette.jpg`) | `/_/admin/dashboard` | ⌘K pressed, "rep" typed. | The command palette finds the replication and Jobs pages after three typed letters. |
| 60 | `reference/admin-ui.md` (new) | `shortcuts-help` | `/_/admin/dashboard` | `?` pressed. | The shortcuts dialog lists the keyboard shortcuts of the admin pages. |
| 61 | `reference/admin-ui.md` (new) | `admin-mobile-drawer` | `/_/admin/jobs` | Viewport 390x844, drawer open. | On a narrow screen the admin navigation opens as a drawer from the left. |

Pages that get no screenshot on purpose: `faq.md` (an index), `reference/s3-api-compatibility.md`, `reference/cli.md`, `reference/iam-permissions.md`, `reference/rate-limits.md`, `reference/capacity-planning.md`, the explanation pages other than the two above, and the terminal-only steps of `tutorials/kubernetes-hello-world.md`, `how-to/deploy-on-kubernetes.md`, `how-to/deploy-with-docker-compose.md` and `how-to/upgrade*.md`. `data-path-architecture.jpg` is a diagram; convert it to Mermaid, which `DocsPage.tsx` already renders (line 95 onward) and which follows the theme, or keep it as one image with a transparent background.

## 3. Producing the screenshots reproducibly

### What exists today

- `demo/s3-browser/ui/playwright.config.ts` runs `./e2e` with Desktop Chrome, `retries: 0`, one worker, and `PLAYWRIGHT_BASE_URL` (default `http://127.0.0.1:19077`).
- `scripts/lib/e2e-proxy.sh` starts one proxy on a free port with a filesystem backend in a temp dir, in `open` or `bootstrap` mode. It sets the admin password `testpass` through a fixed `DGP_BOOTSTRAP_PASSWORD_HASH`, turns off the boot probe (`DGP_BOOT_BACKEND_PROBE=off`), waits on `/_/health`, and exports `PLAYWRIGHT_BASE_URL`.
- `scripts/e2e-smoke.sh` runs `npx playwright test e2e/` against it. The CI job `e2e-smoke` in `.github/workflows/ci.yml:472-524` downloads the release binary artifact, installs Chromium, and runs the script in both modes.
- `scripts/qa-e2e.sh` runs `e2e/qa-regression.spec.ts` in bootstrap mode; the spec skips itself unless `QA_REGRESSION=1` (`qa-regression.spec.ts:38`), so the smoke run does not pick it up. It already has the helpers that a screenshot spec needs: `signIn`, `openBucket`, `createBucketViaUi`, `uploadViaUi`, `openAdmin`, `exportYaml`, `importYaml` (lines 147 to 593), and it seeds data with `@aws-sdk/client-s3`. Nightly runs it in the `qa-e2e` job (`.github/workflows/test-all-nightly.yml:213-242`).
- Theme: `demo/s3-browser/ui/src/ThemeProvider.tsx:16` reads the key `dg-theme` from local storage through `safeStorage.ts`; without a saved choice it follows `prefers-color-scheme`, and it sets `data-theme` on `<html>` (line 41). No existing spec takes screenshots, freezes the clock or sets a color scheme (`grep 'screenshot\|clock\|colorScheme' e2e/` has no match).

### Proposed files

| File | Purpose |
|---|---|
| `scripts/docs-screenshots.sh` | Boots MinIO (the `docker compose` file at the repo root, or the CI service), boots the proxy with the fixture config, seeds it, runs the spec, converts and checks the images. `--update` writes into `docs/screenshots/`; without it the script writes into a temp dir and compares. |
| `scripts/lib/e2e-proxy.sh` | Gains a third mode, `docs`, that writes the fixture config below. The two existing modes do not change. |
| `demo/s3-browser/ui/e2e/docs-screenshots/fixture.yaml` | The fixed-cast config. |
| `demo/s3-browser/ui/e2e/docs-screenshots/seed.ts` | Seeds objects through the S3 API and IAM through the admin API. |
| `demo/s3-browser/ui/e2e/docs-screenshots/shots.ts` | The shot table: one entry per row of section 2. |
| `demo/s3-browser/ui/e2e/docs-screenshots/docs-screenshots.spec.ts` | Runs every shot in both themes. Skips itself unless `DOCS_SCREENSHOTS=1`, like the QA spec, so `e2e-smoke` never runs it. |
| `demo/s3-browser/ui/package.json` | Script `"e2e:docs": "DOCS_SCREENSHOTS=1 playwright test e2e/docs-screenshots"`, and `sharp` as a dev dependency for the WebP conversion. |

### The fixture

The three backends must look real in the Backends screen, so the S3 ones point at MinIO and the filesystem one at a temp dir. The MinIO buckets are created by the script before the proxy starts.

```yaml
access:
  access_key_id: "docs-admin-key"
  secret_access_key: "${env:DOCS_ADMIN_SECRET}"
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: "http://127.0.0.1:9000"   # MinIO, bucket prefix hetzner-
      region: fsn1
      force_path_style: true
      access_key_id: "${env:MINIO_KEY}"
      secret_access_key: "${env:MINIO_SECRET}"
    - name: local-disk
      type: filesystem
      path: "${env:DOCS_DIR}/local-disk"
    - name: aws-dr
      type: s3
      endpoint: "http://127.0.0.1:9000"
      region: eu-west-1
      force_path_style: true
      access_key_id: "${env:MINIO_KEY}"
      secret_access_key: "${env:MINIO_SECRET}"
  default_backend: hetzner-fsn1
  buckets:
    releases:   { backend: hetzner-fsn1 }
    downloads:  { backend: local-disk, public_prefixes: ["public/"] }
    db-archive: { backend: local-disk }
advanced:
  listen_addr: "127.0.0.1:${env:DOCS_PORT}"
```

The IAM state stays in GUI mode, so the seed creates it through the admin API after sign-in: `POST /_/api/admin/users` for `ci-uploader`, `backup-bot` and `dana`, and the groups endpoints for `Engineering`, with a `wait_for_iam_rebuild`-style poll of `GET /_/api/admin/iam/version`. Shots 41 and 42 need declarative mode; the spec switches with a YAML import at the end of the run, so the other shots are not affected. Shot 49 needs `config_sync_bucket` on MinIO, which also sets `DGP_CONFIG_DB_KEY`; the fixture sets both from the start. Shot 38 and 40 need an OIDC issuer; the script serves a static discovery document and JWKS from a local port, which is enough for the form and the sign-in button (the sign-in flow itself is not shot).

The seed writes through the proxy only, never onto the backend, because the repo forbids hand-written DG metadata (source guard `test_fixtures_come_from_the_write_path` in `src/lib.rs`). It uploads `firmware/widget-3000/fw-1.4.0.tar` to `fw-1.4.3.tar` into `releases`, each made from one fixed random seed with a small change per version, so that the delta savings are real and the same on every run. It also writes `reports/build-notes.md`, `downloads/public/installer.sh`, a few `db-archive/nightly/*.dump` files, and runs one replication, one migrate and one re-encrypt so that the Jobs screen has history.

Two shots need special builds. Shot 52 needs `DGP_TEST_FORCE_NONCAS_BACKEND`, which works only in debug builds after the 2.0 change "The `DGP_TEST_*` hooks work only in debug builds"; the script runs that one shot against a debug binary, or the shot is dropped. Shot 22 needs a migrate that stays in its copy phase long enough; the seed uses one large object on `local-disk` and takes the shot as soon as the banner appears.

### The spec

```ts
// demo/s3-browser/ui/e2e/docs-screenshots/docs-screenshots.spec.ts
import { test } from '@playwright/test';
import { SHOTS, type Shot } from './shots';
import { seedOnce, signIn } from './seed';

test.skip(!process.env.DOCS_SCREENSHOTS, 'run with `npm run e2e:docs`');
test.describe.configure({ mode: 'serial' });
test.beforeAll(async () => seedOnce());

for (const theme of ['light', 'dark'] as const) {
  test.describe(theme, () => {
    test.use({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2, colorScheme: theme });
    for (const shot of SHOTS) {
      test(shot.name, async ({ page }) => {
        await page.addInitScript((t) => localStorage.setItem('dg-theme', t), theme);
        await page.clock.setFixedTime(new Date('2026-09-01T10:00:00Z'));
        if (shot.viewport) await page.setViewportSize(shot.viewport);
        await signIn(page, shot.auth ?? 'admin');
        await page.goto(shot.route);
        await shot.setup?.(page);                      // open a dialog, select rows, type
        await page.waitForLoadState('networkidle');
        await page.screenshot({
          path: `${process.env.DOCS_SHOT_DIR}/${shot.name}.${theme}.png`,
          clip: shot.clip, fullPage: shot.fullPage ?? false,
          animations: 'disabled', caret: 'hide',
          mask: [page.locator('[data-shot-mask]'), ...(shot.mask?.(page) ?? [])],
          style: HIDE_VOLATILE_CSS,                    // version chip, relative "3s ago" labels
        });
      });
    }
  });
}
```

The determinism rules:

- The viewport is 1280x800 CSS pixels at device scale 2, so every image is 2560x1600 or a clip of it. The mobile shot sets 390x844. The existing images vary between 1147x1440 and 4148x2352 (section 2).
- `page.clock.setFixedTime` fixes every time that the browser computes, such as "3 minutes ago" in `AuditLogPanel.tsx:126` and `LogsPanel.tsx:72`. Times that the server writes (audit entries, run history) are masked. The plan proposes a `data-shot-mask` attribute on timestamp cells in the UI, so that the spec does not depend on CSS class names.
- The version must never show. The admin page and the dashboard show the running version (`AdminPage.tsx:57`, `MetricsPage.tsx:306`), and the screenshots are served to anonymous callers (see section 4). `HIDE_VOLATILE_CSS` hides the version chip and the build metric, and a check after the run fails if the text of the crate version appears in the page at the time of any shot (`page.textContent('body')`).
- Live numbers (RSS, request rates on the dashboard) are masked; savings numbers are not, because the seed makes them identical on every run.
- Random names are not allowed. The QA spec uses `Date.now()` in bucket names (`qa-regression.spec.ts:34-36`); the screenshot spec uses the cast names only.

### Output, format and budget

The spec writes PNG, and the script converts each file with `sharp` to WebP at quality 80, and keeps the PNG only in the temp dir for the visual diff. The names are `<name>.light.webp` and `<name>.dark.webp` in `docs/screenshots/`. The budget is 150 KB for each file and 12 MB for the whole folder; the script fails when a file or the total goes over. For comparison, the 28 JPG files today use 7.0 MB (`du -sh docs/screenshots`), and `analytics.jpg` alone is 882 KB. The root `.gitignore` ignores `*.png` except under `docs/screenshots/` (lines 62 to 64), so the PNGs in the temp dir are never committed by accident.

### How a page picks the theme

The markdown keeps one theme-neutral reference, `![Full sentence.](/_/screenshots/<name>.webp)`. Both renderers map it to the right variant, because neither renderer can use a `<picture>` element from the markdown the same way:

- The product viewer renders markdown with `react-markdown` without `rehype-raw` (`DocsPage.tsx:388-391`), so raw HTML in the markdown is not rendered as HTML. Its `img` override (`DocsPage.tsx:407-410`) already controls every image; it rewrites `<name>.webp` to `<name>.dark.webp` or `<name>.light.webp` from `useTheme().isDark`, and adds `loading="lazy"`, `width` and `height` to prevent layout shift.
- The marketing site toggles the theme with a `data-theme` attribute and the key `dgp-theme` (`marketing/src/layouts/BaseLayout.astro:42-52`, `marketing/src/components/SiteHeader.astro:79`), so a `<picture>` with a `prefers-color-scheme` media query would ignore the toggle. The rehype pass in `marketing/src/lib/renderDoc.ts:101-102` emits two `<img>` elements with the classes `shot-light` and `shot-dark`, and `marketing/src/styles/docs.css` hides the one that does not match `[data-theme]`.
- The repo root `README.md` renders on GitHub, which supports `<picture>` with `prefers-color-scheme`; it uses that form with the `docs/screenshots/` path.

`DocsLanding.tsx:93,96`, `marketing/src/components/ControlPlane.astro:33-43` and `marketing/src/pages/trial.astro:112` hard-code JPG names today; they move to the new names in the same change.

### How CI keeps them fresh

The repo has no pull requests (repo `CLAUDE.md`, "Git workflow"), so a bot PR is not the right shape. The proposal:

1. A PR-gate check, cheap and without a browser, extends `scripts/check-docs-registry.sh`: every `/_/screenshots/<name>.webp` in `docs/product/**` has both variants on disk, every file in `docs/screenshots/` is referenced by a doc or by one of the named UI and marketing files, every alt text ends with a period and has at least five words, and every file is inside the size budget. Today no check verifies that a referenced image exists (`grep screenshot scripts/` finds only the copy scripts).
2. A job `docs-screenshots` in `.github/workflows/test-all-nightly.yml` builds nothing new (it downloads the release binary like `qa-e2e`), runs `scripts/docs-screenshots.sh` with MinIO, and compares each new PNG with a PNG decoded from the committed WebP using `pixelmatch` at a per-pixel threshold of 0.1. A shot is stale when more than 0.5 percent of its pixels differ. The job uploads the new images and a side-by-side diff as an artifact, and opens or comments on one issue labelled `docs-screenshots-stale`, the same pattern that the nightly already uses for `nightly-failure` (`test-all-nightly.yml:285-306`). A green run closes the issue.
3. The same job runs on `workflow_dispatch`, and on a push to `main` that touches `demo/s3-browser/ui/src/components/**`, `demo/s3-browser/ui/src/theme.css` or the spec, so a UI change shows its effect within the hour and not only at night.
4. The owner refreshes with `scripts/docs-screenshots.sh --update` and commits the files, or downloads the artifact. The marketing workflow already rebuilds on `docs/screenshots/**` (`.github/workflows/marketing-pages.yml:6-8`).

## 4. Where the screenshot files must live

### Current mechanism

| Step | Evidence |
|---|---|
| The single source is `docs/screenshots/`. | `demo/s3-browser/ui/scripts/copy-screenshots.mjs:6-24`, `marketing/scripts/copy-screenshots.mjs:5-19` |
| The UI build copies it to `demo/s3-browser/ui/public/screenshots/` (git-ignored), and Vite copies that to `dist/screenshots/`. | `demo/s3-browser/ui/package.json` scripts `predev`, `prebuild`, `sync:screenshots`; root `.gitignore:65-68` |
| rust-embed bakes `dist/` into the binary, and `build.rs` emits one `rerun-if-changed` per file, so a re-shot image with the same name is re-embedded. | `src/demo.rs:18-20`, `build.rs:8-25` |
| The binary serves `/_/screenshots/<file>` from the embedded assets without a session, with `Cache-Control: no-cache` (only `assets/` is immutable). | `src/demo.rs:583-594` |
| The markdown is embedded separately and served behind a session at `/_/api/docs`, so the docs never enter the public JS bundle. | `src/demo.rs:22-28`, `src/demo.rs:672-693`, `demo/s3-browser/ui/src/docsBundle.ts:1-17` |
| The marketing build copies `docs/screenshots/` into `marketing/public/screenshots/` (git-ignored) and rewrites `/_/screenshots/` to `/screenshots/` in every image source. | `marketing/scripts/copy-screenshots.mjs:18-34`, `marketing/.gitignore:23-25`, `marketing/src/lib/docs.ts:117-121`, `marketing/src/lib/renderDoc.ts:101-102` |
| Both copy scripts accept `.jpg`, `.jpeg`, `.png`, `.webp` and `.svg`. | `marketing/scripts/copy-screenshots.mjs:24` (the UI script has the same filter) |

### Recommendation

Keep `docs/screenshots/` as the only committed location and keep the URL form `/_/screenshots/<name>.webp` in the markdown. Do not put images under `docs/product/`, and do not commit them into either `public/` folder.

What breaks with other choices:

- Images under `docs/product/` would be embedded twice (once by `ProductDocs`, `src/demo.rs:27`, once through `dist/` if they are also copied), would sit behind the session-gated docs API that returns markdown text, and would be picked up by the Vite glob in `marketing/src/lib/docContent.ts:10` only for `*.md`, so the website would not serve them.
- A relative path in the markdown (such as `../screenshots/x.webp`) would not survive either surface: the product viewer renders from an in-memory string, and the marketing site serves the doc under a different URL than its source path. `rewriteAssetSrc` matches only the prefix `/_/screenshots/` (`marketing/src/lib/docs.ts:120`).
- The binary grows with every image, because `dist/` is embedded whole. `dist/` is 14 MB today, and 7.0 MB of that is screenshots. Two themes of 60 shots as JPG would add about 15 MB; as WebP within the budget they stay under 12 MB in total. That is why the budget check is part of the plan.
- `scripts/check-bundle-fingerprints.sh` scans only `*.js`, `*.html`, `*.css` and `*.json` under `dist/` (line 41), so it never sees an image. But the screenshots are served to anonymous callers from the same `dist/` (`src/demo.rs:583-594`). A screenshot that shows the version, the build time or a real hostname would therefore fingerprint the build, which is exactly what that check exists to stop. The spec's version check (section 3) closes this gap; alternatively the fingerprint check could run OCR, which is heavier and not proposed.
- Changing the file extension changes nothing in rust-embed or in the copy scripts (both accept WebP), but `mime_guess` must map `.webp` to `image/webp` (`src/demo.rs:585`); the plan verifies it with one request in the pipeline's smoke step.

## 5. Roadmap

The phases are in order. Sizes are rough: small is under a day, medium is one to three days, large is more than three days of focused work.

| Phase | Work | Size |
|---|---|---|
| 0. Guardrails | Extend `scripts/check-docs-registry.sh` with the image checks from section 3, a check that bold UI paths match `ADMIN_IA` groups and labels, and an em-dash lint outside code blocks (as a warning list first, an error after phase 1). Add one upstream bucket name to the fixed cast in the repo `CLAUDE.md`. | Small |
| 1. Accuracy and humanizer pass | Install humanizer; rewrite page by page under the three rules in section 1. Fix `how-to/upgrade.md`, the stale UI paths, the off-cast names, the "Deferred" list in `reference/lifecycle.md`, and the Coolify detail in `troubleshooting.md`. Check every page against `CHANGELOG.md` "## Unreleased" for 2.0 behaviour changes (log level `info`, `503` for a missing config DB, opaque continuation tokens, `config lint` on an empty file). | Medium to large |
| 2. Screenshot pipeline | `scripts/docs-screenshots.sh`, the `docs` mode in `e2e-proxy.sh`, the fixture, the seed, the spec with ten shots to prove it, the `data-shot-mask` attributes, the theme-aware `img` in `DocsPage.tsx`, the two-image rehype output and CSS on the marketing site, the WebP and budget step. | Large |
| 3. Capture | The remaining shots of section 2, re-named and with sentence alt texts; delete `advanced_security.jpg` and the other replaced JPGs, and update `DocsLanding.tsx`, `ControlPlane.astro`, `trial.astro` and the root `README.md`. | Medium |
| 4. Structure | Split `reference/configuration.md`; move the admin-api sections; split `how-to/backend-capability-validation.md`; add the explanation pages; make `upgrade.md` version-neutral; add the "which page do I need" tables to the Kubernetes pages; add redirects for renamed slugs in `marketing/astro.config.mjs` (`REDIRECTS`). Update `manifest.json` in the same commits (`scripts/check-docs-registry.sh` enforces parity). | Large |
| 5. New pages | The three tutorials (each executed end to end before merge, as the repo `CLAUDE.md` requires for tutorials), the nine how-to pages, and `reference/admin-ui.md`. | Large |
| 6. Freshness in CI | The nightly and on-UI-change `docs-screenshots` job with the visual diff and the stale-screenshots issue. | Small to medium |

Phases 1 and 2 can run in parallel, because one edits prose and the other adds tooling. Phase 3 needs phase 2. Phases 4 and 5 are best after phase 3, so the new pages get their shots when they are written.

## Success criteria

- Every page in `docs/product/` (except the changelog) is exactly one Diátaxis type, and a reviewer can name it from the title.
- No page mentions a version older than 1.19 except the changelog and the "older upgrades" section of `how-to/upgrade.md`.
- Every page that shows a command or YAML block uses only names from the fixed cast.
- `reference/configuration.md` is under 300 lines, and no reference fact is kept in two reference pages.
- There are at least 50 screenshots, each in a light and a dark variant, each referenced by at least one page, each with a full-sentence alt text, and none over 150 KB. The folder stays under 12 MB.
- `scripts/docs-screenshots.sh --update` on a clean checkout reproduces every committed image within the visual-diff threshold.
- No screenshot shows the crate version, a build time, a real hostname or a real secret.
- The PR gate fails on a missing image, an unreferenced image, a UI path that does not exist, and an em dash in prose.
- The nightly opens an issue within a day of a UI change that makes a screenshot stale.
- A person who follows the tutorials in order ends with a proxy that has three backends, a replication rule, a lifecycle rule and IAM managed as code, without reading any how-to page.
