# DeltaGlider Proxy docs

This tree splits into two audiences, enforced by CI so it can't drift:

- **[product/](product/)** — operator-facing, bundled into the running binary at `/_/docs/`. Install, configure, secure, run, debug. If you operate an instance, this is what you read.
- **[dev/](dev/)** — contributor-facing, **never** bundled. Build from source, release workflow, CI infrastructure, historical design docs.

`screenshots/` is shared — the same images ship in the binary (via the UI build/static asset pipeline) and render on GitHub. The marketing site also copies from this directory at build time and fails if the required product screenshots are missing or smaller than 900×700:
`filebrowser.jpg`, `analytics.jpg`, `iam.jpg`, `advanced_security.jpg`,
`bucket-policies.jpg`, and `object-replication.jpg`.

## Product docs index

See [product/README.md](product/README.md) — that file is also the landing page inside the running binary. The structure follows [Diátaxis](https://diataxis.fr): every page is exactly one of tutorial / how-to / reference / explanation:

1. **Start here** — the three tutorials (first delta savings, securing the proxy, Helm on kind) + the FAQ index
2. **Guides: deploy & operate** — how-to recipes: production checklist, Compose/Helm, TLS, upgrades, backups, HA, monitoring, tracing, troubleshooting
3. **Guides: storage & data** — routing, migration (into the proxy and between backends), compression & quotas, replication, lifecycle, encryption, events
4. **Guides: access & security** — IAM users, conditions, SSO, IAM-as-code, admission rules, public folders
5. **Reference** — pure facts: configuration, CLI, admin API, authentication, IAM permissions, rate limits, encryption, jobs, replication, lifecycle, event outbox, declarative IAM, metrics
6. **Concepts** — explanations: delta compression, multi-backend routing, the security model, encryption at rest, jobs & durability

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
