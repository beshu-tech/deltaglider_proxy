# How to set up OAuth/OIDC single sign-on

*Let humans sign in to the admin UI with the identity provider they already have, and land in the right IAM group automatically.*

## Prerequisites

- The proxy reachable at a URL the identity provider can redirect to — `https://s3.acme.example` in production. `http://localhost:9000` works for testing against providers that accept localhost callbacks.
- Admin access to the UI (bootstrap password or an IAM admin like `dana`).
- An IAM group to map people into — this guide uses `Engineering` (see [How to create IAM users and groups](create-iam-users.md)).

## 1. Register the app with your provider

Every provider needs the same redirect URL — note there is **no provider-name suffix**; the callback is generic:

```
https://s3.acme.example/_/api/admin/oauth/callback
```

If you use Google (Cloud Console):

1. APIs & Services → Credentials → Create credentials → OAuth client ID.
2. Application type: **Web application**.
3. Authorized redirect URIs: add the callback URL above.
4. Save, then copy the **Client ID** and **Client Secret**.
5. The issuer URL is `https://accounts.google.com`.

If you use Okta:

1. Applications → Create App Integration → **OIDC**, **Web Application**.
2. Sign-in redirect URIs: the callback URL above.
3. Assignments: pick the Okta groups that should be allowed to log in.
4. Copy the **Client ID** and **Client Secret**.
5. The issuer URL is the URL of your Okta authorization server. For the default custom server it is `https://<your-org>.okta.com/oauth2/default`; for the org server it is `https://<your-org>.okta.com`.

If you use Azure AD / Entra:

1. App registrations → New registration; Redirect URI (Web): the callback URL above.
2. Certificates & secrets → New client secret. Copy the value immediately — Azure hides it on the next page load.
3. API permissions → Microsoft Graph → `openid`, `profile`, `email`; add `GroupMember.Read.All` if you'll map on AD groups.
4. Copy the **Application (client) ID** and the secret value.
5. The issuer URL is `https://login.microsoftonline.com/<tenant-id>/v2.0`, where `<tenant-id>` is the **Directory (tenant) ID** on the app's overview page.

If you use any other OIDC provider: it works as long as it serves `.well-known/openid-configuration`. Collect the issuer URL, client ID, client secret, and scopes (at minimum `openid email`; add `profile` and `groups` if you map on them).

## 2. Add the provider in the admin UI

Go to **Settings → Access → External authentication** → **Add provider**.

The proxy has one provider type, `oidc`. Google, Okta, and Azure AD have no type of their own: each one is an OpenID Connect issuer, so you add it as an `oidc` provider with its issuer URL. The proxy reads the issuer's `.well-known/openid-configuration` document and takes the authorization, token, and key endpoints from it. The form sets the type to `oidc` for you. In the admin API and in declarative YAML, `provider_type` must be `oidc`. The proxy refuses a provider of any other type when you save it (`422` in the admin API, a refused apply in declarative mode), because it has no sign-in flow for another type.

| Field | Value |
|---|---|
| Provider name | a unique identifier, for example `okta` |
| Display name | the label of the sign-in button (`Sign in with Okta`). When it is empty, the button shows the provider name. |
| Issuer URL | the issuer URL from step 1 |
| Client ID / Client secret | from step 1 |
| Scopes | `openid email profile` minimum; add `groups` per your provider |
| Enabled | ✓ |

The form has no priority field. In the admin API and in YAML, `priority` sets the order of the buttons on the login page: a provider with a higher number is shown first.

Click **Test Connection**, before or after you save. The proxy tests the values that are in the form at that moment and saves nothing. A blank client secret keeps the saved secret for the test. The proxy fetches the issuer's `.well-known/openid-configuration` and reports DNS, TLS, or connectivity problems before any human tries to log in. The error names the underlying cause, for example `invalid peer certificate: UnknownIssuer` for a certificate from a CA that the proxy does not trust. Test Connection also works in declarative IAM mode, because it changes nothing. In the admin API, `POST /_/api/admin/ext-auth/providers/:id/test` tests a saved provider, and an optional JSON body with the form fields replaces the saved values for that test only. `POST /_/api/admin/ext-auth/providers/test` tests a provider that is not saved yet. A test that fails answers `200` with `success: false` and the reason in `error`.

The proxy checks the issuer URL when you save the provider. By default the issuer must use `https://` and a public address, because the proxy refuses requests to private, loopback, and cloud-metadata addresses. A provider that breaks this rule is refused with `422` and a message that names the rule. The client secret is never returned in a response: the API shows `****` in its place.

### An identity provider in a private network

If your identity provider (for example Keycloak or Dex) runs on a private address, or uses a certificate from a private CA, set two keys in the provider's `extra_config`:

| Key | Value | Effect |
|---|---|---|
| `allow_local` | `true` | The issuer, and the endpoints that its discovery document names, may use `http://` and private addresses. This is the same opt-in as `allow_local` on backends and on event delivery. Cloud-metadata addresses stay refused. |
| `ca_cert_path` | path to a PEM file | The certificates in the file are added to the trust roots for this provider. The proxy checks at save time that the file holds at least one certificate. |

In the admin UI, the provider form sets them in its **Network** fields: the **Allow http:// and private addresses** switch sets `allow_local`, and the **CA certificate file** field sets `ca_cert_path`. The form keeps the other keys of `extra_config` when you save. If the proxy refuses the provider with `422`, the form shows the reason under the fields, and the provider stays as it was.

In the admin API, the provider body carries them as `"extra_config": {"allow_local": true, "ca_cert_path": "/etc/deltaglider/idp-ca.pem"}`. In declarative YAML they go under the provider entry:

```yaml
access:
  auth_providers:
    - name: corp-keycloak
      provider_type: oidc
      client_id: deltaglider
      client_secret: "${env:DGP_KEYCLOAK_SECRET}"
      issuer_url: "https://keycloak.corp.internal/realms/acme"
      scopes: "openid email profile"
      extra_config:
        allow_local: true
        ca_cert_path: /etc/deltaglider/idp-ca.pem
```

The proxy does not use the operating system's certificate store for identity providers. It trusts the public web roots that are built into it, plus the file in `ca_cert_path`.

## 3. Map IdP groups to IAM groups

Mapping rules decide which IAM groups an identity joins, based on its claims. Acme maps the Okta `engineering` group to the `Engineering` IAM group.

Go to **Settings → Access → External authentication** → **Allowed Users & Group Assignment** → **Add Rule**, fill in the new row, and click **Save Rules**.

![OAuth provider and group mapping settings](/_/screenshots/oauth_group_mapping.jpg)

| Field | Value |
|---|---|
| Match type | **Claim value** |
| Claim field | `groups` |
| Match value | `engineering` |
| Assign to group | `Engineering` |
| Provider | the provider of step 2, or **All providers** |

The match types are **Email pattern** (`*` matches any characters, for example `*@acme.example`), **Email domain**, **Email exact**, **Email regex**, and **Claim value**. A claim-value rule matches when the claim is a string equal to the value, or a list that contains the value, without regard to case. It does not support wildcards. Rules that read the email match only when the provider marks the email as verified.

The proxy checks every rule at each login, and the identity joins the group of every rule that matches. Rule order does not change the result.

Common claim fields: Google `hd` (hosted domain); Okta `groups`; Azure AD `groups` (UUIDs) or `roles` for app roles. For an email match, use one of the email match types.

Use the **Preview** box before you rely on a rule: type an email address and click **Check**, and the UI shows which groups that address would receive. Preview checks only the email: it cannot show the result of a claim-value rule. Group memberships are merged on each login, never replaced, so manual assignments survive SSO.

A login needs no matching rule. The proxy creates the user on the first login in any case, and a user that no rule matches has no group memberships and no permissions.

## 4. First login

Open the login page in a private window. A "Sign in with Okta" button now appears:

![OAuth login screen](/_/screenshots/oauth_login.jpg)

Have `dana` click it. She authenticates at the provider, consents, and is redirected back. On success:

- A row appears under **Login Activity** on the **External authentication** page, linking the provider's subject ID to a DeltaGlider user.
- Matching mapping rules fire — `dana` is now a member of `Engineering`.
- She gets a session cookie and lands in the file browser. Only a user with admin permissions (direct or through a group) gets an admin session. Every other user gets a browser-only session, which opens the file browser with the user's own S3 permissions and refuses the admin API with `403`. The bulk copy, move, delete, and ZIP actions of the file browser work in that session, under the same permissions.

Every successful OAuth login shows as `external_login` in **Settings → Observability → Audit log**. A failed login shows an error page in the browser and a line in the proxy log.

## If the login fails

Three failure modes are provider-side, not proxy-side:

1. **`invalid_redirect_uri` at the provider.** The registered URI doesn't byte-for-byte match `https://s3.acme.example/_/api/admin/oauth/callback` — watch trailing slashes and `http` vs `https`. If a reverse proxy fronts DeltaGlider, also confirm it forwards the same `Host` header the user sees. The proxy builds the callback URL from the `Host` header. It uses `X-Forwarded-Host` and `X-Forwarded-Proto` only when `DGP_TRUST_PROXY_HEADERS=true` and the request comes from a network in `DGP_TRUSTED_PROXY_CIDRS`, because otherwise any client could choose the host that receives the authorization code.
2. **Azure `groups` claim missing.** Azure AD omits groups by default; in the app registration go to Token configuration → Add groups claim, then retry the flow.
3. **"Code exchange failed" on the error page, or a failed Test Connection.** The proxy couldn't reach the provider. Read the error first: it names the cause (DNS, a refused connection, a refused private address, or a TLS error such as `UnknownIssuer`). A `curl` from the proxy container is not a reliable check. `curl` trusts the operating system's certificate store and connects to private addresses, but the proxy does neither unless you set `ca_cert_path` and `allow_local` (see [An identity provider in a private network](#an-identity-provider-in-a-private-network)). So `curl` can succeed while the proxy fails. If `curl` fails too, fix DNS, the network, or the certificate first; it isn't an OAuth problem.

Two refusals come from the proxy itself. First, the proxy refuses a login whose email the provider does not mark as verified (an ID token without the `email_verified` claim counts as unverified), and the error page says so. To accept such logins, set `require_email_verified: false` in the provider's `extra_config`; email-based mapping rules still do not match such an identity. Second, after too many failed sign-ins from one address, the proxy locks that address out for a while. The authorize and callback pages then answer `429` with a `Retry-After` header, and the error page names the wait, for example "Try again in 10 min.".

If login succeeds but the user has no permissions, no mapping rule matched — check with the Preview tool, and verify the auto-created user row in **Settings → Access → Users**. If you rotate the client secret at the provider, update it in the provider form; it takes effect on save, no restart.

## Verify

1. Log in via the provider as a user in the mapped IdP group — expect a session and membership of `Engineering` (visible on the user's row).
2. Log in as a user **outside** the mapped group — expect a login that lands with no group memberships (or no login at all, if the provider restricts assignments).
3. Confirm both attempts in the audit log.

## Related

- [Authentication reference](../reference/authentication.md) — auth modes, claim handling, error responses.
- [How to create IAM users and groups](create-iam-users.md) — the groups your mapping rules target.
- [How to manage IAM as code](manage-iam-as-code.md) — providers and mapping rules can live in YAML too.
- [About authentication and access control](../explanation/security-model.md) — how OAuth layers on bootstrap and IAM.
