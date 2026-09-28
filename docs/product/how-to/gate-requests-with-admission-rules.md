# How to gate requests before authentication

*Reject unwanted traffic, such as requests from bad IPs, anonymous writes, or all requests during maintenance, before the proxy computes a single HMAC for it.*

Request rules run before the proxy verifies the request signature. Because of that, they can do something that IAM cannot do: they can refuse a request without knowing who sent it, including a request that carries no credentials at all. The page [About authentication and access control](../explanation/security-model.md) explains why the rules run first.

In the configuration file, request rules are the entries of the `admission.blocks` list. This page says "rule", as the admin UI does.

## 1. Add the rule

Acme's `downloads` bucket serves a public prefix, which attracts anonymous upload attempts. This rule denies all anonymous changes on the whole bucket.

In the admin UI:

1. In the sidebar, open **Access → Request rules** (`/_/admin/access/admission`), and click **Add rule**.
2. In the form **Add request rule**, type `deny-anonymous-writes-downloads` in **Name**. Under the conditions, tick `PUT`, `POST` and `DELETE` in **HTTP methods**, type `downloads` in **Bucket**, and select **Anonymous only** in **Signed request**. Leave **Source IPs** and **Object key pattern** empty, so that they match any value.
3. Under **Then**, select **Deny (403)**.
4. Click **Add rule** in the form.

   ![The Add request rule form holds the rule deny-anonymous-writes-downloads; callout 1 marks the Add rule button of the page, callout 2 the conditions, callout 3 the Deny (403) action under Then, and callout 4 the Add rule button of the form.](/_/screenshots/admission-add-rule.webp)

5. The new rule is now at the end of **Your rules**. To move it, drag it by its handle (**Drag to reorder**) to its place in the list (see section 2).
6. Click **Review & apply** in the bar at the bottom of the page.

   ![The new rule deny-anonymous-writes-downloads is at the end of the list, and the page holds an unsaved change; callout 5 marks the drag handle of the new rule, and callout 6 marks the Review & apply button.](/_/screenshots/admission-reorder-apply.webp)

7. Check the diff in the dialog, and then click **Apply and Persist**.

   ![The review dialog shows the change to the list of request rules; the box marks the Apply and Persist button.](/_/screenshots/admission-apply-dialog.webp)

The form has no YAML view. The pencil button of a rule opens the same form to edit it, and **Remove rule…** in the menu of a rule removes it from the page. Nothing changes on the proxy until you review and apply. The proxy loads the new rules without a restart.

The other actions under **Then** are **Allow without credentials** (`allow-anonymous`), **Reject with a custom status** (`reject`, with a **Status code** and a **Response message**), and **Continue to authentication** (`continue`).

### The same change in YAML

The steps above write this configuration into the `admission` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). In the file, request rules are the list under the `admission.blocks` key, and each entry is one rule:

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

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

A request matches a rule only when it matches every condition of the rule. A rule with an empty `match: {}` matches every request. The proxy checks the rules for every request, so the list holds at most 1000 of your rules. A config with more rules is refused, on load and on every apply, with an error that names the count and the limit. The [configuration reference](../reference/configuration.md#admission-chain) lists all conditions (`method`, `source_ip` or `source_ip_list` with CIDR networks, `bucket`, `path_glob`, `authenticated`) and all actions (`deny`, `allow-anonymous`, `continue`, and `reject` with a custom status and message).

An `allow-anonymous` rule lets the matched request through without credentials only when that request is a read: a `GET` or `HEAD` of an object, or a listing with the requested prefix. The rule grants exactly that request and nothing wider. A `GET` or `HEAD` of a key that contains `*` or `?` gets no grant and is refused with `403`, because the proxy cannot write a permission that names only that key. A write that matches an `allow-anonymous` rule is refused with `403`, because the proxy never grants a write to an anonymous caller.

## 2. Put the rules in order

The proxy checks the rules from the top of the list to the bottom, and the **first rule that matches decides**. A request that matches rule 1 never reaches rule 2. For this reason, put narrow exceptions above broad rules. For example, an `allow-anonymous` rule for one path must be above a `deny` rule that would otherwise match the same requests.

This is the order of the two example rules of Acme after the apply:

![The Request rules page lists two rules that run before authentication; callouts 1 and 2 mark them in the order that the proxy checks them.](/_/screenshots/request-rules.webp)

Your rules are always checked before the public-access rules (see the next section). Because of this order, one `deny` rule can take a published public folder offline, and you do not have to change the bucket settings.

## The public-access rules

Below your rules, the Request rules page shows read-only rules whose names start with `public-prefix:`. The proxy creates these rules from the public access setting of each bucket (`public_prefixes`). They give anonymous users **read-only** access. To change them, use **Storage → Buckets** (`/_/admin/storage/buckets`), not this page. The `public-prefix:` name prefix is reserved, so your own rules cannot use it.

## 3. Dry-run with trace

Test the rules before you rely on them. The trace checks a sample request against the rules of the **running** proxy and shows which rule decided. It does not send real traffic.

From the CLI:

```bash
export DGP_BOOTSTRAP_PASSWORD=...
deltaglider_proxy admission trace --method PUT --path /downloads/public/installer.zip \
  --server https://s3.acme.example | jq .
```

The result is a `deny` decision that names the rule `deny-anonymous-writes-downloads`. Run the command again with `--authenticated`. This time the rule does not match, because its `authenticated: false` condition is not true, and the request continues to SigV4 authentication.

The same tool is in the admin UI:

1. In the sidebar, open **Observability → Request rule tester** (`/_/admin/diagnostics/trace`).
2. Select `PUT` in **Method**, type `/downloads/public/installer.zip` in **Path**, leave **Authenticated** off, and click **Test request**. **Decision** shows `deny` by the rule `deny-anonymous-writes-downloads`.

   ![The rule tester shows that an anonymous PUT of downloads/public/installer.zip is denied; the box marks the decision and the rule deny-anonymous-writes-downloads that made it.](/_/screenshots/admission-trace-deny.webp)

The result also shows the **Reason path**, the **Resolved request**, and a **Copy as JSON** button. For every `allow-anonymous` decision it also shows an **Anonymous access** box, which names what the anonymous caller may do: the one object that it may read, the bucket and prefix that it may list, or the public prefixes of the bucket. For a write, the box says that nothing is granted. The CLI result carries the same information in the `anonymous_grant` field.

If you want the trace to name a rule also for requests that match no other rule, add a `continue` rule at the end of the list. A `continue` rule sends the request on to authentication, so it changes nothing. Its only purpose is to make the trace output explicit.

## 4. Roll out

Apply the change with **Review & apply** (section 1), or commit the `admission:` section to your configuration file and apply it with `deltaglider_proxy config apply`. Then watch **Observability → Audit log** (`/_/admin/diagnostics/audit`) for a few minutes. Each denied request shows there with its source IP and path, so a rule that matches too much becomes visible quickly.

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

- [Configuration reference](../reference/configuration.md#admission-chain): every condition and action field.
- [How to publish a folder publicly](publish-a-public-folder.md): where the public-access rules come from.
- [How to restrict access by IP and prefix](restrict-access-with-conditions.md): per-user IP rules *after* authentication.
- [About authentication and access control](../explanation/security-model.md): admission's place in the four-layer model.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
