# OAuth / OIDC setup

*Wire up Google, Okta, Azure AD, or any generic OIDC provider for admin login and group auto-assignment.*

DeltaGlider Proxy supports OAuth/OIDC for two things:

1. **Single sign-on** into the admin UI — users click "Sign in with Google" instead of typing the bootstrap password.
2. **Automatic group assignment** via mapping rules — when a new identity lands, rules match on their claims (email domain, group membership) and the user is added to the right IAM groups without operator intervention.

Conceptual background and full field reference live in [reference/authentication.md](../reference/authentication.md#oauthoidc-mode). This page is the task-oriented walkthrough.

## Prerequisites

- DeltaGlider Proxy running at a **publicly reachable URL** (the identity provider needs to call the callback). Localhost works for testing against providers that accept `http://localhost:9000/_/api/admin/oauth/callback`.
- Admin access to the UI (bootstrap password or an existing IAM admin).
- A client ID + client secret from the identity provider — we'll get that in step 1.

## Step 1: Register the OAuth application with your provider

The redirect URL you give the provider is:

```
https://<your-dgp-host>/_/api/admin/oauth/callback
```

Note: **no provider-name suffix** — the callback is generic; the proxy figures out which provider a given state belongs to from the in-flight request.

### Google (Cloud Console)

1. APIs & Services → Credentials → Create credentials → OAuth client ID.
2. Application type: **Web application**.
3. Authorized redirect URIs: add `https://<your-dgp-host>/_/api/admin/oauth/callback`.
4. Save. Copy the **Client ID** and **Client Secret**.

### Okta

1. Applications → Create App Integration → **OIDC**, **Web Application**.
2. Sign-in redirect URIs: `https://<your-dgp-host>/_/api/admin/oauth/callback`.
3. Assignments: pick the groups that should be allowed to log in.
4. Copy **Client ID** and **Client Secret**.

### Azure AD / Entra

1. App registrations → New registration.
2. Redirect URI (Web): `https://<your-dgp-host>/_/api/admin/oauth/callback`.
3. Certificates & secrets → New client secret. Copy the value immediately — Azure hides it on next load.
4. API permissions → Microsoft Graph → `openid`, `profile`, `email`, optionally `User.Read` and `GroupMember.Read.All` if you plan to map on AD groups.
5. Copy **Application (client) ID** and the secret value.

### Generic OIDC

Any provider with a `.well-known/openid-configuration` endpoint works. You'll need:

- Issuer URL (e.g. `https://login.example.com`)
- Client ID
- Client secret
- Scopes (at minimum `openid email`; add `profile` and `groups` if you map on them)

## Step 2: Add the provider in the admin UI

Admin Settings → **Configuration** → **Access** → **External authentication** → **+ Add provider**.

| Field | Value |
|---|---|
| Name | lower-case ascii id — this shows up in the sign-in button (`Sign in with google`) |
| Display name | human-readable ("Google Workspace"), shown on the login page |
| Provider type | `google` / `okta` / `azure` / `oidc` |
| Issuer URL | for `oidc`, the issuer; for named providers, pre-filled |
| Client ID | from step 1 |
| Client secret | from step 1 |
| Scopes | `openid email profile` minimum; add `groups` or `User.Read` per your provider |
| Enabled | ✓ |
| Priority | lower number = shown first on the login page |

Click **Save**. The UI immediately shows "Sign in with &lt;provider&gt;" on the login page.

**Test the provider** with the right-side menu → Test. The proxy calls `.well-known/openid-configuration` on the issuer and reports any connectivity / TLS / DNS problems.

## Step 3: Add group mapping rules

Identity claims from the provider determine which IAM group(s) a user lands in. Example: "every identity with `hd: beshu.com` in their Google claims gets added to the `developers` IAM group."

Admin Settings → **Access** → **External authentication** → **Mapping rules** → **+ Add rule**.

| Field | Meaning |
|---|---|
| Name | Free-text, descriptive ("beshu-staff-developers") |
| Priority | Rules evaluate in ascending priority; first match wins |
| Match | Claim path + value pattern. Supports exact match and glob (`*`, `?`) |
| Target groups | IAM groups the identity is added to when the rule matches |

**Common claim paths:**

| Provider | Claim | Example match |
|---|---|---|
| Google | `hd` (hosted domain) | `beshu.com` |
| Google | `email` | `*@beshu.com` |
| Okta | `groups` | `s3-admins` |
| Azure AD | `groups` | `c1b1… (UUID)` or use `roles` for app roles |

Use the **Preview** button: type an email / claim set, and the UI shows which groups that identity would land in. Cheaper than logging in as them.

## Step 4: First login

On the login page, click the provider button. You're redirected to the provider's consent screen, then back to `/_/api/admin/oauth/callback`. On success:

- A new row appears in **External identities** (Access → External authentication → Identities) linking the provider's subject ID to a DeltaGlider user.
- Group memberships from matching rules are applied.
- The user gets a session cookie; they land in the admin UI.

Audit log entries (`/_/admin/diagnostics/audit`) show `external_login` for every successful OAuth login. Denials show as `access_denied`.

## Troubleshooting

**"invalid_redirect_uri" from the provider.** Either the URI registered with the provider doesn't exactly match `https://<your-dgp-host>/_/api/admin/oauth/callback` (watch trailing slashes and http vs https), or your reverse proxy is sending the proxy a different Host header than the one the user sees.

**Login succeeds but the user has no permissions.** No mapping rule matched. Check the Preview tool with the identity's claims. Also confirm the IAM user row that got created (Access → Users) and manually add them to a group as a test.

**"Token exchange failed" in the audit log.** The proxy couldn't reach the provider's token endpoint. From the proxy container, `curl -v https://<issuer>/.well-known/openid-configuration` — if that fails, it's a network/DNS/TLS problem, not an OAuth one.

**Microsoft Graph `groups` claim missing.** Azure AD doesn't include groups by default. In the app registration, Token configuration → Add groups claim, then restart the flow.

**"Client secret rotated, now nothing works."** Update the secret in Access → External authentication → edit provider. The proxy picks up the new secret on save (no restart).

## Related

- [Reference: authentication](../reference/authentication.md) — concepts, supported claim shapes, error responses.
- [IAM conditions](32-iam-conditions.md) — restrict which source IPs or prefixes an identity can touch.
- [SigV4 and IAM users](31-sigv4-and-iam.md) — OAuth is one auth mode; SigV4 is the other.
