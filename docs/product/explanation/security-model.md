# About authentication and access control

DeltaGlider Proxy sits between every client and every byte of stored data, so every request must pass through it. The security model uses that position. Requests pass through a stack of layers in a fixed order, and each layer answers exactly one question. When you know why each layer sits where it does, you can predict how the system behaves, and you know where to put a given rule.

## Four layers, evaluated in order

Every request follows the same path: admission, then authentication, then authorization. The public-prefix carve-outs are part of the admission chain.

Admission comes first because it is cheap and needs no identity. Request rules (the `admission.blocks` list) are rules that you write. They match on method, source IP or CIDR, bucket, and path glob. The proxy evaluates them from top to bottom, the first match wins, and all of this happens *before* the proxy verifies any signature. This order is deliberate. Signature verification costs HMAC computations and a credential lookup. A rejection because a request came from a blocklisted IP costs one CIDR comparison. Acme may want the admin surface reachable only from the office network (`203.0.113.0/24`), or want anonymous writes to the `downloads` bucket refused outright. Those rules do not need to know who the caller is, so the proxy should not spend work to find out. Admission is also the place for a maintenance-mode reject (503 with a human-readable message) that applies to every request, whatever its credentials.

![The Request rules page lists two rules that run before authentication; callouts 1 and 2 mark them in the order that the proxy checks them.](/_/screenshots/request-rules.webp)

SigV4 and sessions answer "who are you." After a request passes admission, the proxy verifies its identity: SigV4 signatures (header auth or presigned URLs) for the S3 API, and session cookies for the admin GUI. Verification is constant-time, with clock-skew tolerance and replay detection for mutating requests. Identity and permission are separate steps on purpose. A valid signature from `ci-uploader` proves only that the request came from `ci-uploader`.

IAM ABAC answers "what may you do." When the identity is known, the proxy evaluates the user's permission rules: actions (`read`, `write`, `delete`, `list`, `admin`) against resources (`bucket/key` patterns), with Deny beating Allow. This is where Acme's CI pipeline gets its narrow grant. `ci-uploader` is allowed `write` on `releases/*` and nothing else, so a compromised CI token cannot read `db-archive` or delete anything. Group permissions merge with direct ones: the `Engineering` group carries read+list on `releases/*`, and every member inherits it. One rule is less strict on purpose. When a prefix-scoped user LISTs wider than their grant, the proxy admits the request and post-filters the results. It does not reject the request. Here the proxy prefers discoverability to strictness: AWS S3 refuses such a LIST, but a user who cannot find their own keys files a ticket.

Public prefixes are carve-outs, not a fifth credential type. Acme publishes installers from the `public/` prefix of the `downloads` bucket. The proxy creates public-access request rules (`public-prefix:*`) from the bucket settings, and checks them *after* your own rules. An unauthenticated GET under the prefix runs as a built-in `$anonymous` user with scoped read+list permissions. The same ABAC machinery therefore applies, and every anonymous access writes a line to the proxy log. Two invariants keep this safe. Anonymous requests can never write. Credentials always win: a request that carries a valid SigV4 signature gets full IAM evaluation, whatever the public-prefix config says. Anonymous grants must never widen what an authenticated user can do. A client that presents credentials shows that it intends to act as that identity, and the proxy treats the request that way.

The order also explains one interaction. The proxy checks your request rules before the public-access rules, so a `deny` rule can override a public prefix. You can use this to take a published prefix offline with one rule, without a change to the bucket config.

## Modes that grow, not switches

The proxy refuses to start without credentials unless you explicitly opt into open access. From there, authentication grows through three stages. Each stage activates itself when the proxy's state supports it, without a configuration flag.

**Bootstrap** is a single shared SigV4 credential pair from YAML or env vars. It suits the first day: a single-tenant service, one CI pipeline, and no per-user audit. **IAM mode** activates automatically when the first IAM user is created. The first user can come from the admin GUI, declarative YAML, or OAuth auto-provisioning. In `gui` mode, the proxy carries the bootstrap pair over as a `legacy-admin` user, so existing clients keep working during the change. In `declarative` mode, the YAML file lists every user, so the bootstrap pair keeps working only when one of the YAML users carries it. **OAuth/OIDC** adds sign-in for people on top. At Acme, an Okta sign-in auto-provisions an IAM user, and group mapping rules put that user into `Engineering`. A new engineer therefore gets read access to `releases` without a change in the admin panel. The proxy merges group memberships on each login and does not replace them, so manual assignments survive SSO. Only a user with admin permissions gets an admin session from SSO. Every other user gets a browser-only session that uses the user's own S3 permissions.

A mode switch would need a flag day: when you flip it, every client breaks until you reconfigure it. With auto-activation, the system is always in the most capable mode that its state supports, and old credentials keep working through the transition. You migrate by adding users, and you do not have to schedule downtime.

Two infrastructure secrets sit under all of this, and they are separate on purpose. The **bootstrap password** signs admin session cookies and controls access to the GUI in bootstrap mode. The **config DB key** (`DGP_CONFIG_DB_KEY`, or a key file that the proxy generates on the first start) encrypts the SQLCipher config DB. Earlier releases used the bootstrap password hash as the DB key. That coupling had two costs: anyone who could read the config file could decrypt the IAM database, and a password reset made the database unreadable. With a separate key, a password reset is harmless, and the key never has to appear in a config file. The cost is one more secret to back up. The config DB key is the master key for your IAM data, so protect it and back it up.

## Verify, then re-sign

The proxy never forwards a client's signature to the backend. SigV4 binds the signature to the Host header and URI path, so a signature made for the proxy is invalid at the backend. Instead, the proxy verifies the client's signature, discards it, and issues its own freshly signed requests with the backend credentials.

This makes the proxy the client's *only* trust boundary, on purpose. Clients hold proxy credentials (`ci-uploader`'s key pair). The proxy holds backend credentials (the `hetzner-fsn1` API keys). An attacker with a stolen client credential can do exactly what that IAM user can do through the proxy. The attacker gets no direct backend access and no access to another user's scope. It also means that you can rotate backend credentials without a change to any client. You can also point the same client at `local-disk` or `aws-dr` tomorrow, and the client does not know that anything changed.

## GUI-managed vs declarative IAM

By default, the encrypted config DB is the source of truth for users, groups, and providers, and you manage them in the admin GUI. Setting `access.iam_mode: declarative` moves authority to YAML. The proxy reconciles the file into the DB on every apply, and admin-API IAM mutations return 403, so runtime drift cannot happen.

Declarative mode is the better choice when IAM changes should be pull requests. Every grant is then reviewed in git, and multiple replicas converge from the same file. A compliance question is answered by `git log` on a YAML file, with no forensics inside a database. GUI mode is the better choice when you manage IAM in the GUI every day, or when most of your users are OAuth-created external identities. Those bindings live only in the DB, and YAML never expresses them.

The reconciler has three safeguards for the GitOps path. Diff-by-name means that the reconciler matches entities by name, not by DB row id. Rotating `dana`'s access key is therefore an UPDATE that preserves her row, so her Okta identity binding survives the rotation. Validation runs before any write, so a duplicate access key or an unknown group reference fails the whole apply with zero state change. The empty-YAML gate blocks the most dangerous mistake. A flip from `gui` to `declarative` with no users in the YAML would wipe a populated DB without a warning, so the proxy refuses the flip with an error.

## What the audit ring is and is not

Every security-relevant admin action (logins, IAM mutations, reconciler changes, and IAM denials of S3 requests) goes into an in-memory ring buffer (500 entries by default). The admin GUI shows the ring with a delay of a few seconds. Its purpose is to answer "what just happened?" while you work in the GUI: who logged in, which reconcile ran, which request IAM refused. Anonymous reads of public prefixes go only to the proxy log, not to the ring.

It is not a compliance log. It is bounded and in memory, and a restart clears it. The proxy also emits the same events to stdout through structured logging. If you need a durable, tamper-evident audit, ship those logs to a system that is durable and tamper-evident. The ring is a convenience mirror, and it is cheap on purpose.

## Open mode, honestly

`authentication: none` removes identity: the proxy serves every request without a user behind it. It does not skip the signature check, though. The proxy still needs a secret to read signed and chunked uploads, so it checks the signature of every signed request with the access key as the secret. A client that signs with the same value for both keys (for example `dummy` / `dummy`) is served, and a client that signs with a real key pair gets `403 SignatureDoesNotMatch`. An unsigned request is served. Open mode is fine on localhost, in a dev loop where signing requests adds work and gives nothing back. Anywhere else, turn auth on. Open mode exposes every object to anyone who can reach the port, and a plan to "add auth later" is how such a port ends up on the internet. For this reason, the proxy makes you type the setting explicitly.

## Related

- Tutorial: [Secure your proxy](../tutorials/secure-your-proxy.md)
- How-to: [Gate requests with admission rules](../how-to/gate-requests-with-admission-rules.md)
- How-to: [Manage IAM as code](../how-to/manage-iam-as-code.md)
- Reference: [Authentication and access](../reference/authentication.md)
- Reference: [IAM permissions and conditions](../reference/iam-permissions.md)
