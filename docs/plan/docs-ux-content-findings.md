# Docs UX pass: content findings (2026-09-27)

The docs-ux pass fixed rendering, styling and navigation in code. These are
the problems it found in the markdown content itself. The docs-refresh branch
owns those files, so the lead applies the fixes after both branches merge.

Evidence: a browser pass of all 59 docs on the website and in the product,
light and dark theme, at 1440 px and 390 px, plus the two new anchor tests
(`marketing/src/lib/anchors.test.ts`, `demo/s3-browser/ui/src/__tests__/docAnchors.test.ts`).
After the merge, run both tests: they fail on any `#anchor` link that the
refreshed content breaks.

| # | Page | Section | Problem | Suggested fix |
|---|------|---------|---------|---------------|
| 1 | `CHANGELOG.md` → `docs/product/changelog.md` | 1.x entries (CHANGELOG.md lines 5329, 5331, 5340, 5411) | Four relative links point at repository files: `docs/plan/progressive-config-refactor.md`, `docs/plan/admin-ui-revamp.md` (two links), `docs/HOWTO_MIGRATE_TO_YAML.md`. None of the three files exists in the repo any more, and the links resolve against `docs/product/`, so they are 404 on the website and dead in the product. | Remove the links and keep the words as plain text. For the YAML migration, link `how-to/upgrade.md` instead. Then run `scripts/gen-changelog-doc.sh`. |
| 2 | `tutorials/first-delta-savings.md` | Opening paragraph | The website landing cards and `/llms.txt` show each page's first paragraph as its summary. Here that is the story ("Acme Robotics ships a new firmware build for the Widget 3000…"), which does not say what the reader will learn. | Open with a one-sentence italic summary, as the README and the explanation pages do: *Run the proxy, upload two firmware versions, and see the second one stored as a small delta.* |
| 3 | `tutorials/secure-your-proxy.md` | Opening paragraph | Same as 2: the summary is "This tutorial continues exactly where Your first delta savings left off…". | Italic summary first: *Replace open access with real credentials and a least-privilege IAM user.* |
| 4 | `tutorials/kubernetes-hello-world.md` | Opening paragraph | Same as 2, and the cut lands mid-parenthesis: "…a real chart on a real (if d…". | Italic summary first: *Install the Helm chart on a local kind cluster and prove it stores and returns a file.* |
| 5 | `reference/configuration.md` | "Environment variable registry" | Two headings repeat earlier headings: `### Server / Advanced` and `### Delta engine` exist as `##` headings in the field reference too. The TOC shows two identical entries each, and the second copies get the ids `server--advanced-1` and `delta-engine-1`, so a link to "the env vars of the delta engine" cannot be written by name. | Rename the registry headings: `### Server / Advanced variables`, `### Delta engine variables` (and the same pattern for the other registry groups). |
| 6 | `reference/configuration.md` | Top of page | A hand-written list of `#` links repeats the automatic "On this page" TOC that both surfaces now show (right rail on desktop, folding list under the title on phones). | Remove the hand-written list, or cut it to the four or five sections a reader needs most. |
| 7 | `reference/metrics.md` | Two `### Histogram buckets` headings | The same heading under two different `##` sections gives two identical TOC entries. | The first is under `## HTTP requests`, the second under `## Delta compression`: use `### HTTP request histogram buckets` and `### Delta compression histogram buckets`. |
| 8 | `reference/cli.md` | `##` headings with full synopses | Headings such as `admission trace --method <M> --path <P> [--authenticated] [--query <Q>] [--server <URL>] [--timeout <SECS>]` fill three lines on a phone and four lines in the TOC. | Name the command in the heading (`## admission trace`) and put the synopsis in a code block under it. |
| 9 | `how-to/backend-capability-validation.md` | Title | "How to use non-CAS backends safely (backend capability validation)" wraps to three lines in both sidebars, and "non-CAS" is jargon in a title. | "How to use a backend without conditional writes" (keep the current title's terms in the first paragraph for search). |
| 10 | `README.md` (repo root) and `docs/product/README.md` | Docs index / "Pick your path" | Nothing points readers who use an AI assistant at the new LLM files. | Add this text (see below). |

## Text for finding 10

For the repo `README.md`, in the documentation section:

```markdown
**Using an AI assistant?** Give it [llms.txt](https://deltaglider.com/llms.txt)
(an index of every docs page) or [llms-full.txt](https://deltaglider.com/llms-full.txt)
(all docs in one file). Every docs page is also plain markdown at its URL plus
`.md`, for example <https://deltaglider.com/docs/reference/configuration.md>.
```

For `docs/product/README.md` (shipped in the product, so it must not depend on
the website being reachable), at the end of "Pick your path":

```markdown
To give a page to an AI assistant, use **Copy page** at the top of the page: it
copies the page as markdown. The public website also publishes all pages for
LLMs at <https://deltaglider.com/llms.txt>.
```
