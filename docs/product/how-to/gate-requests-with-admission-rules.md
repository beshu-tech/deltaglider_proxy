# How to gate requests before authentication

*Reject unwanted traffic — bad IPs, anonymous writes, everything during maintenance — before the proxy spends a single HMAC on it.*

Request rules run before the proxy verifies the request signature. Because of that, they can do something that IAM cannot do: they can refuse a request without knowing who sent it, including a request that carries no credentials at all. The page [About authentication and access control](../explanation/security-model.md) explains why the rules run first.

In the configuration file, request rules live in the list under the `admission.blocks` key. Each entry in that list is one rule. The rest of this page says "rule", as the admin UI does.

## 1. Add a rule

Acme's `downloads` bucket serves a public prefix, which attracts anonymous upload attempts. Deny anonymous mutations on the whole bucket outright.

In the admin UI, open **Settings → Access → Request rules** and click **Add rule**. Set the conditions that a request must match, choose the action, and save the rule. Then drag the rule to its position in the list. The rule editor has a form view and a YAML view.

![Request rules editor](/_/screenshots/admission-rules.jpg)

This is the same rule in YAML:

```yaml
# validate
admission:
  blocks:
    - name: deny-anonymous-writes-downloads
      match:
        method: [PUT, POST, DELETE]
        bucket: downloads
        authenticated: false
      action: deny
```

A request matches a rule only when it matches every condition of the rule. A rule with an empty `match: {}` matches every request. The [configuration reference](../reference/configuration.md#admission-chain) lists all conditions (`method`, `source_ip` or `source_ip_list` with CIDR networks, `bucket`, `path_glob`, `authenticated`) and all actions (`deny`, `allow-anonymous`, `continue`, and `reject` with a custom status and message).

An `allow-anonymous` rule lets the matched request through without credentials only when that request is a read: a `GET` or `HEAD` of an object, or a listing with the requested prefix. The rule grants exactly that request and nothing wider. A write that matches an `allow-anonymous` rule is refused with `403`, because the proxy never grants a write to an anonymous caller.

## 2. Put the rules in order

The proxy checks the rules from the top of the list to the bottom, and the **first rule that matches decides**. A request that matches rule 1 never reaches rule 2. For this reason, put narrow exceptions above broad rules. For example, an `allow-anonymous` rule for one path must be above a `deny` rule that would otherwise match the same requests.

Your rules are always checked before the public-access rules (see the next section). Because of this order, one `deny` rule can take a published public folder offline, and you do not have to change the bucket settings.

## The public-access rules

Below your rules, the Request rules page shows read-only rules whose names start with `public-prefix:`. The proxy creates these rules from the public access setting of each bucket (`public_prefixes`). They give anonymous users **read-only** access. To change them, use **Settings → Storage → Buckets**, not this page. The `public-prefix:` name prefix is reserved, so your own rules cannot use it.

## 3. Dry-run with trace

Test the rules before you rely on them. The trace checks a sample request against the rules of the **running** proxy and shows which rule decided. It does not send real traffic.

From the CLI:

```bash
export DGP_BOOTSTRAP_PASSWORD=...
deltaglider_proxy admission trace --method PUT --path /downloads/public/installer.zip \
  --server https://s3.acme.example | jq .
```

The result is a `deny` decision that names the rule `deny-anonymous-writes-downloads`. Run the command again with `--authenticated`. This time the rule does not match, because its `authenticated: false` condition is not true, and the request continues to SigV4 authentication.

The same tool is in the admin UI at **Settings → Observability → Request rule tester**. It shows the decision, the rule that matched, and ready-made example requests, and it has a Copy-as-JSON button:

![Request trace diagnostics](/_/screenshots/request-trace.jpg)

If you want the trace to name a rule also for requests that match no other rule, add a `continue` rule at the end of the list. A `continue` rule sends the request on to authentication, so it changes nothing; it exists only to make the trace output explicit.

## 4. Roll out

Apply the change with the bar at the bottom of the admin UI page. Alternatively, commit the `admission:` section to your configuration file and apply it with `deltaglider_proxy config apply`. The proxy loads the new rules without a restart. Then watch **Settings → Observability → Audit log** for a few minutes. Each denied request shows there with its source IP and path, so a rule that matches too much becomes visible quickly.

## Verify

```bash
# Anonymous PUT — expect 403
curl -sw "%{http_code}\n" -o /dev/null -X PUT \
  https://s3.acme.example/downloads/public/installer.zip --data-binary @installer.zip

# Anonymous GET under the public prefix — still works (the rule matches only writes and deletes)
curl -sw "%{http_code}\n" -o /dev/null \
  https://s3.acme.example/downloads/public/installer-1.2.0.zip
```

Run the trace from step 3 again after each change to the rules. The trace reads the rules of the running proxy, so it also confirms that the change is live.

## Related

- [Configuration reference](../reference/configuration.md#admission-chain) — every condition and action field.
- [How to publish a folder publicly](publish-a-public-folder.md) — where the public-access rules come from.
- [How to restrict access by IP and prefix](restrict-access-with-conditions.md) — per-user IP rules *after* authentication.
- [About authentication and access control](../explanation/security-model.md) — admission's place in the four-layer model.
