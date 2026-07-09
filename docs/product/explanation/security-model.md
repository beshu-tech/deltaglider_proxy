# About authentication and access control

DeltaGlider Proxy sits between every client and every byte of stored data, which makes it a chokepoint by design. The security model embraces that: instead of one monolithic "is this allowed?" check, requests pass through a stack of layers, each answering exactly one question, in a fixed order. Understanding why each layer sits where it does makes the whole system predictable — and makes it obvious where to put any given rule.

## Four layers, evaluated in order

Every request walks the same path: admission, then authentication, then authorization, with public-prefix carve-outs woven into the admission chain.

**Admission comes first because it's cheap and identity-free.** Admission blocks are operator-authored rules — match on method, source IP or CIDR, bucket, path glob — evaluated top-to-bottom, first match wins, *before* any signature is verified. That ordering is the point. Signature verification costs HMAC computations and a credential lookup; rejecting a request because it came from a blocklisted IP costs a CIDR comparison. When Acme wants the admin surface reachable only from the office network (`203.0.113.0/24`), or wants anonymous writes to the `downloads` bucket refused outright, those rules don't need to know who the caller is — so they shouldn't pay for finding out. Admission is also where you put a maintenance-mode reject (503 with a human-readable message) that fires on everything, regardless of credentials.

![Request rules](/_/screenshots/admission-rules.jpg)

**SigV4 and sessions answer "who are you."** Once a request survives admission, the proxy verifies its identity: SigV4 signatures (header auth or presigned URLs) for the S3 API, session cookies for the admin GUI. Verification is constant-time, with clock-skew tolerance and replay detection for mutating requests. Identity and permission are deliberately separate steps — a valid signature from `ci-uploader` proves the request came from `ci-uploader`, nothing more.

**IAM ABAC answers "what may you do."** With identity established, the proxy evaluates the user's permission rules: actions (`read`, `write`, `delete`, `list`, `admin`) against resources (`bucket/key` patterns), with Deny beating Allow. This is where Acme's CI pipeline gets its narrow grant — `ci-uploader` is allowed `write` on `releases/*` and nothing else, so a compromised CI token can't read `db-archive` or delete anything. Group permissions merge with direct ones: the `Engineering` group carries read+list on `releases/*`, and every member inherits it. One deliberate softness: when a prefix-scoped user LISTs wider than their grant, the proxy admits the request and post-filters the results rather than rejecting. Discoverability wins over strictness here — it matches AWS's own behavior, and a user who can't find their own keys files a ticket either way.

**Public prefixes are carve-outs, not a fifth credential type.** Acme publishes installers from the `public/` prefix of the `downloads` bucket. Rather than minting a shared "anonymous" credential, the proxy synthesizes admission blocks (`public-prefix:*`) from per-bucket config, evaluated *after* any operator-authored blocks. An unauthenticated GET under the prefix runs as a synthesized `$anonymous` user with scoped read+list permissions — so the same ABAC machinery applies, and every anonymous access is audit-logged. Two invariants keep this safe: anonymous requests can never write, and credentials always win — a request carrying valid SigV4 gets full IAM evaluation regardless of public-prefix config. Anonymous grants must never widen what an authenticated user can do; presenting credentials signals intent to act as that identity, and the proxy takes you at your word.

The ordering also explains an interaction worth knowing: because operator admission blocks fire before the synthesized ones, an admission `deny` can shadow a public prefix. That's a feature — it's how you take a published prefix offline in one rule without touching bucket config.

## Modes that grow, not switches

The proxy refuses to start without credentials unless you explicitly opt into open access. From there, authentication grows through three stages — and the stages activate themselves rather than being configuration flags.

**Bootstrap** is a single shared SigV4 credential pair from YAML or env vars. It's the right shape for day one: a single-tenant service, one CI pipeline, no per-user audit. **IAM mode activates automatically when the first IAM user is created** — via the admin GUI, declarative YAML, or OAuth auto-provisioning — and the bootstrap pair is carried over as a `legacy-admin` user so nothing breaks mid-flight. **OAuth/OIDC** layers on top for humans: at Acme, Okta sign-in auto-provisions an IAM user and group mapping rules drop them into `Engineering`, so a new engineer gets read access to `releases` without anyone touching the admin panel. Group memberships are merged on each login, not replaced — manual assignments survive SSO.

Why auto-activation instead of a mode switch? Because a switch implies a flag day: flip it and every client breaks until reconfigured. Auto-activation means the system is always in the most capable mode its state supports, and old credentials keep working through the transition. You migrate by adding users, not by scheduling downtime.

One opinionated design sits underneath all of this: the **bootstrap password** is a single secret with three jobs — it encrypts the SQLCipher config DB, signs admin session cookies, and gates the GUI in bootstrap mode. The trade is deliberate: one infrastructure secret to manage and back up, in exchange for a real blast radius if you reset it (a reset invalidates the encrypted IAM database; the safe rotation path re-encrypts atomically). Treat it like a master key, because it is one.

## Verify, then re-sign

The proxy never forwards a client's signature to the backend — it can't, and wouldn't want to. SigV4 binds the signature to the Host header and URI path, so a signature minted for the proxy is invalid at the backend anyway. Instead the proxy verifies the client's signature, discards it, and issues its own freshly signed requests using backend credentials.

This makes the proxy the client's *only* trust boundary, on purpose. Clients hold proxy credentials (`ci-uploader`'s key pair); the proxy holds backend credentials (the `hetzner-fsn1` API keys). Compromising a client credential gets you exactly what that IAM user could do through the proxy — never direct backend access, never another user's scope. It also means you can rotate backend credentials without touching a single client, and point the same client at `local-disk` or `aws-dr` tomorrow without it knowing anything changed.

## GUI-managed vs declarative IAM

By default the encrypted config DB is the source of truth for users, groups, and providers, and you manage them in the admin GUI. Setting `access.iam_mode: declarative` flips authority to YAML: the file is reconciled into the DB on every apply, and admin-API IAM mutations return 403 so runtime drift simply cannot happen.

Declarative wins when IAM changes should be pull requests: every grant reviewed in git, multiple replicas converging from the same file, compliance answered by `git log` on a YAML file instead of forensics inside a database. It loses when the GUI is your daily driver, or when OAuth-born external identities are your primary surface — those bindings live only in the DB and are never expressed in YAML.

The reconciler is built to make the GitOps path hard to hurt yourself with. **Diff-by-name** means entities are matched by name, not DB row id: rotating `dana`'s access key is an UPDATE that preserves her row, so her Okta identity binding survives the rotation. **Validation runs before any write** — a duplicate access key or an unknown group reference fails the whole apply with zero state change. And the **empty-YAML gate** refuses the one catastrophic foot-gun: flipping from `gui` to `declarative` with no users in the YAML would silently wipe a populated DB, so the proxy refuses loudly instead.

## What the audit ring is — and isn't

Every security-relevant action — logins, IAM mutations, anonymous accesses, reconciler changes — lands in an in-memory ring buffer (500 entries by default) that the admin GUI renders with a few seconds' latency. It exists to answer "what just happened?" while you're standing in the GUI: who logged in, which reconcile ran, what `$anonymous` fetched.

It is not a compliance log. It's bounded, in-memory, and gone on restart. The same events are emitted to stdout via structured logging; if you need durable, tamper-evident audit, ship those logs somewhere that is. The ring is a convenience mirror, deliberately cheap.

## Open mode, honestly

`authentication: none` disables SigV4 verification entirely. It's fine on localhost — a dev loop where signing requests is friction with no payoff. Anywhere else, turn auth on. There is no nuance to add: open mode exposes every object to anyone who can reach the port, and "we'll add auth later" is how that port ends up on the internet. The proxy makes you type the setting explicitly for exactly this reason.

## Related

- Tutorial: [Secure your proxy](../tutorials/secure-your-proxy.md)
- How-to: [Gate requests with admission rules](../how-to/gate-requests-with-admission-rules.md)
- How-to: [Manage IAM as code](../how-to/manage-iam-as-code.md)
- Reference: [Authentication and access](../reference/authentication.md)
- Reference: [IAM permissions and conditions](../reference/iam-permissions.md)
