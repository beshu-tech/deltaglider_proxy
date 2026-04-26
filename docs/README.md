# DeltaGlider Proxy docs

This tree splits into two audiences, enforced by CI so it can't drift:

- **[product/](product/)** — operator-facing, bundled into the running binary at `/_/docs/`. Install, configure, secure, run, debug. If you operate an instance, this is what you read.
- **[dev/](dev/)** — contributor-facing, **never** bundled. Build from source, release workflow, CI infrastructure, historical design docs.

`screenshots/` is shared — the same images ship in the binary (via `demo/s3-browser/ui/public/screenshots/`) and render on GitHub. The marketing site also copies from this directory at build time and fails if the required product screenshots are missing or smaller than 900×700:
`filebrowser.jpg`, `analytics.jpg`, `iam.jpg`, `advanced_security.jpg`,
`bucket-policies.jpg`, and `object-replication.jpg`.

## Product docs index

See [product/README.md](product/README.md) — that file is also the landing page inside the running binary. Grouped by operator journey:

1. **Start here** — quickstart, setting up a bucket
2. **Deploy to production** — deployment, security checklist, upgrade guide
3. **Authentication & access** — OAuth, SigV4, IAM, rate limiting
4. **Day 2 operations** — monitoring, troubleshooting, FAQ
5. **Reference** — config fields, admin API, metrics, internals

## Dev docs

- [contributing.md](dev/contributing.md) — build, test, project structure
- [releasing.md](dev/releasing.md) — release process, git tagging, Docker pipeline
- [ci-infra.md](dev/ci-infra.md) — k3s runner setup, actions-runner-controller
- [historical/](dev/historical/) — design docs that shaped the current implementation; kept for archaeology, not maintained

## CI enforcement

Three blocking checks in [.github/workflows/ci.yml](../.github/workflows/ci.yml) keep this tree honest:

1. **`scripts/check-docs-registry.sh`** — every `.md` under `docs/product/` must be imported in [demo/s3-browser/ui/src/docs-imports.ts](../demo/s3-browser/ui/src/docs-imports.ts). Anything under `docs/dev/` that sneaks into the registry fails CI.
2. **`scripts/check-docs-yaml-examples.sh`** — every fenced `yaml` block marked `# validate` must pass `deltaglider_proxy config lint`. Prevents example drift.
3. **lychee** — every inter-doc link resolves; no broken references.
