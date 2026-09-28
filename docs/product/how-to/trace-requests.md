# How to trace and audit requests

This guide shows you how to find out why the proxy allowed or denied a request. You test a sample request against the request rules, read the audit log, and turn on debug headers.

## 1. Check the audit log first

When a client reports a denial, start at **Observability → Audit log** (`/_/admin/diagnostics/audit`). Type a user, an address, a bucket or a path into the filter field to narrow the list.

![The audit log lists recent admin actions with the user, the source address and the target of each one; the box marks the filter field.](/_/screenshots/audit-log.webp)

 Every IAM denial goes there with the user, action, bucket, and path. Usually that is all the information that you need. The same data is available as JSON:

```bash
curl -b cookies "https://s3.acme.example/_/api/admin/audit?limit=500"
```

If the audit log shows nothing for the failing request, the denial happened **before** IAM, in SigV4 verification or in the request rules. Use tracing to find it.

The audit log is an **in-memory** ring buffer (default 500 entries, `DGP_AUDIT_RING_SIZE` to raise it) that is empty again after every restart. The ring size exists only as an environment variable; the YAML file and the admin UI cannot set it. The persistent audit source is stdout. Every `audit_log()` call also emits a `tracing::info!` line. Ship those lines into your log pipeline for retention.

## 2. Trace a synthetic request

Three front doors lead to the same evaluator, and none of them touches real data:

**Admin UI:** open **Observability → Request rule tester** (`/_/admin/diagnostics/trace`). Fill in **Method**, **Path**, and optionally **Query string** and **Source IP**, set **Authenticated**, and click **Test request**. The panel shows the decision and the reason path, and **Copy as JSON** copies the whole result.

![The rule tester shows that an anonymous GET of downloads/public/installer.sh is allowed; the box marks the decision and the rule that made it.](/_/screenshots/rule-tester.webp)

**CLI:**

```bash
DGP_BOOTSTRAP_PASSWORD=... deltaglider_proxy admission trace \
  --method PUT --path /downloads/public/tool.zip \
  --server https://s3.acme.example | jq
```

Add `--authenticated` to simulate a signed request, and `--query` for query strings. The password comes from the env var and not from a flag, because argv is visible in `ps`.

**API:** `POST /_/api/admin/config/trace` with a synthetic request body, or the `GET` query-param variant for bookmarkable trace URLs:

```bash
curl -b cookies "https://s3.acme.example/_/api/admin/config/trace?method=PUT&path=/downloads/public/tool.zip"
```

## 3. Read the reason path

The trace output is a decision plus the path that produced it: the decision tag (`allow-anonymous`, `deny`, `reject` or `continue`, which means that no rule decided and the request goes on to authentication), the matched rule by name, and the resolved request as the evaluator saw it. The first matching rule decides, so the named rule is the complete answer. The evaluator did not check any rule after it.

When the decision is `allow-anonymous`, the output also says what the rule lets a caller without credentials do. The API returns this in the `anonymous_grant` field, and the admin UI shows it in an **Anonymous access** box. The rule grants only reads: a `GET` or `HEAD` of the matched object, a listing of the matched bucket with the requested prefix, or, for a public-access rule, the public prefixes of the bucket. A write is never granted. So a `PUT` that matches an `allow-anonymous` rule shows no grant, and the box says that the request continues without credentials and is refused with `403 AccessDenied`.

Worked example: `downloads` has a public prefix:

```yaml
# validate
storage:
  buckets:
    downloads:
      public_prefixes:
        - public/
```

- Trace `GET /downloads/public/tool.zip`, unauthenticated → **allow-anonymous**, matched rule `public-prefix:downloads`. This is the public-access rule that the proxy creates from the bucket setting. It allows reading and listing only.
- Trace `PUT /downloads/public/tool.zip`, unauthenticated → **continue**, with no matched rule. The public-access rule matches only read methods, so the PUT goes on to authentication, which refuses an anonymous PUT with `403`.

The prefix is the same and the outcomes are opposite. For each outcome, the trace names the rule that decided it, or it shows that no rule matched.

## 4. Turn on debug headers

For per-request visibility on real traffic, set `DGP_DEBUG_HEADERS=true` and read the response headers. The variable exists only in the environment; the YAML file and the admin UI cannot set it:

- `x-amz-storage-type`: how the object is stored: `delta`, `passthrough`, or `reference`.
- `x-deltaglider-stored-size`: on the responses to object requests, the number of bytes that the object takes on the backend, which is smaller than the object size for a delta.
- `x-deltaglider-listing-facts-misses`: on every LIST, the number of entries on the page that show their stored size instead of their original size, because the proxy found no listing facts for them (see [how delta compression works](../explanation/delta-compression.md)).

Turn this **off** in production when you are done, because it reveals storage internals to anyone who can send a request.

## Verify

```bash
# Trace agrees with reality: this should print an allow-anonymous decision...
DGP_BOOTSTRAP_PASSWORD=... deltaglider_proxy admission trace \
  --method GET --path /downloads/public/tool.zip --server https://s3.acme.example | jq .admission.decision

# ...and the real request behaves the same way
curl -s -o /dev/null -w "%{http_code}\n" https://s3.acme.example/downloads/public/tool.zip
```

Then make one failing authenticated request on purpose and confirm it appears in the audit log within seconds.

## Related

- [Troubleshooting](troubleshooting.md): symptom-indexed fixes once you know which layer denied
- [Security model](../explanation/security-model.md): admission → SigV4 → IAM, in order
- [Admin API reference](../reference/admin-api.md): trace and audit endpoints
- [CLI reference](../reference/cli.md): `admission trace` flags and exit codes
