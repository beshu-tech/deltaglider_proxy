# Securing your first proxy

*Replace open access with real credentials and a least-privilege IAM user.*

This tutorial continues exactly where [Your first delta savings](first-delta-savings.md) left off: a proxy running on `localhost:9000` in open-access mode, with a `releases` bucket holding two firmware versions.

Right now, anyone who can reach port 9000 can read, overwrite, or delete everything in that bucket. That was fine for a first look, but not for any other use. By the end of this tutorial, the proxy will have an admin password that you chose, and S3 requests will require real credentials. Acme's CI pipeline will have its own key that can write firmware builds *and nothing else*. You will also see the brute-force rate limiter block a password-guessing attack.

## Step 1: set your own admin password

In tutorial 1 we ran with `DGP_AUTHENTICATION=none`, so there is no admin password yet, and the proxy is wide open. Before we turn authentication on, we set a bootstrap password that we control. The `--set-bootstrap-password` flag reads a password from stdin and writes its bcrypt hash onto our data volume. (If we had started *with* auth enabled, the proxy would have generated a random password and printed it once, but only to an interactive terminal. We set our own password in either case.)

Stop the running proxy first: press `Ctrl+C` in the terminal where the container is running.

Now set the new password. Pick your own, with 12 characters minimum:

```bash
printf '%s\n' 'acme-rocks-mauve-42' | docker run --rm -i -v dgp-data:/data \
  beshultd/deltaglider_proxy --set-bootstrap-password
```

You should see a confirmation:

```
Bootstrap password hash written to .deltaglider_bootstrap_hash
The IAM database is not affected (its key is DGP_CONFIG_DB_KEY or the key file).

For Docker/env vars (base64, no escaping needed):
  DGP_BOOTSTRAP_PASSWORD_HASH=JDJiJDEyJ...
```

The IAM database that the first run created is still usable: its encryption key is in the key file `deltaglider_config.db.key` on the same volume, and that key does not depend on the password.

Start the proxy again with the same command as before. It stays in open mode for a few more minutes:

```bash
docker run --rm -it -p 9000:9000 -v dgp-data:/data \
  -e DGP_AUTHENTICATION=none \
  beshultd/deltaglider_proxy
```

Notice that there is no password box in the logs this time. The proxy found our hash on the volume and used it.

Now prove the password works. Open [http://localhost:9000/_/admin](http://localhost:9000/_/admin). You should see an **Admin Login** gate that says **Enter the admin password to continue.** Type `acme-rocks-mauve-42` (or the password that you chose) and sign in. The admin settings open, with a navigation sidebar on the left.

## Step 2: require S3 authentication

Now we close the open door. The proxy is still in open mode because we started it with `DGP_AUTHENTICATION=none`. An environment variable wins over the settings, so the **S3 authentication mode** choice on **Access → Credentials & mode** is read-only right now, with a note that names the variable. For this reason, this step changes the environment and not the admin UI: we restart the proxy without that variable and with a bootstrap credential pair instead.

Stop the running proxy (`Ctrl+C`), and start it again:

```bash
docker run --rm -it -p 9000:9000 -v dgp-data:/data \
  -e DGP_ACCESS_KEY_ID=acme-admin \
  -e DGP_SECRET_ACCESS_KEY=correct-horse-battery-staple-acme-1 \
  beshultd/deltaglider_proxy
```

Sign in to [http://localhost:9000/_/admin](http://localhost:9000/_/admin) again with your password, because a restart ends every session. Then look at the result in the admin UI:

1. In the sidebar, open **Access → Credentials & mode** (`/_/admin/access/credentials`).
2. Under **S3 authentication mode**, **Auto-detect (recommended)** is now selected, and the banner above it says that SigV4 authentication is on.
3. The **Bootstrap SigV4 credentials** card shows the access key ID `acme-admin`. Because the value comes from `DGP_ACCESS_KEY_ID`, the field is read-only and carries a badge that says so.

   ![The Credentials & mode page; callout 1 marks Auto-detect (recommended) under S3 authentication mode, and callout 2 marks the Bootstrap SigV4 credentials card, which holds the access key ID of the proxy.](/_/screenshots/secure-credentials-mode.webp)

   The screenshot comes from a proxy whose key is in the config file, so its field has no environment badge.

Test the new rule from the second terminal, where the `dummy` credentials from tutorial 1 are still exported:

```bash
aws --endpoint-url http://localhost:9000 s3 ls
```

```
An error occurred (AccessDenied) when calling the ListBuckets operation: Access Denied
```

The proxy accepted the dummy credentials an hour ago, and now it rejects them. Switch to the real pair and try again:

```bash
export AWS_ACCESS_KEY_ID=acme-admin
export AWS_SECRET_ACCESS_KEY=correct-horse-battery-staple-acme-1

aws --endpoint-url http://localhost:9000 s3 ls
```

```
2026-06-12 10:31:02 releases
```

The proxy and the bucket are the same, but now only signed requests with the right key get in.

### The same change in YAML

The two environment variables set the `access` section of the configuration ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). On a proxy without these variables, you can put the pair in `deltaglider_proxy.yaml` instead:

```yaml
# validate
access:
  access_key_id: acme-admin
  secret_access_key: correct-horse-battery-staple-acme-1
```

When the variables are not set, you can also type the pair into the **Bootstrap SigV4 credentials** card and click **Review & apply**, and then **Apply and Persist**. The environment variables `DGP_ACCESS_KEY_ID` and `DGP_SECRET_ACCESS_KEY` override the file, and the admin UI then shows both fields as read-only. After you edit the file, restart the proxy, or apply the file to the running proxy with `deltaglider_proxy config apply deltaglider_proxy.yaml --server http://localhost:9000`.

## Step 3: create the `ci-uploader` user

One shared credential is better than none, but Acme's CI pipeline should not hold the keys to everything. Next, we give it its own identity, scoped to the firmware folder. Users live in the encrypted config database, so the admin UI is the place to create one, and there is no YAML for this step. (To manage users in YAML, see [How to manage IAM as code](../how-to/manage-iam-as-code.md).)

In the admin UI:

1. In the sidebar, open **Access → Users** (`/_/admin/access/users`). Because no IAM users exist yet, the page says **Create the first user**. Notice the note under it: your current credentials are kept as an admin user, so nothing that you just set up breaks.
2. Click **New** above the user list.
3. In **Name**, type `ci-uploader`. Leave **Access Key ID** and **Secret Access Key** empty, so that the proxy generates them.
4. Edit the rule under **Permissions**. A new user starts with one rule that allows **List** and **Read** everywhere. Keep **Allow**, replace `*` under **WHERE** with `releases/firmware/*`, and click **Write** under **CAN DO**, so that **List**, **Read** and **Write** are on.
5. Click **Create User**.

   ![The Users page with the form of the new ci-uploader user; callout 1 marks Users in the sidebar, callout 2 the New button, callout 3 the Name field, callout 4 the rule that allows List, Read and Write on releases/firmware/*, and callout 5 the Create User button.](/_/screenshots/iam-users-new-form.webp)

   The screenshot comes from a proxy that already has more users, so its list is not empty.

You should see a dialog titled **User created: save these credentials**, showing the generated access key and secret. Copy both now, because the proxy shows the secret only this once.

Notice the user list: `ci-uploader` shows **1 rule**, and a second row, `legacy-admin`, shows **Full admin**. That row is your `acme-admin` credential pair, carried over as a proper IAM user.

## Step 4: verify least privilege

A permission rule that you only saw succeed is half-tested. We test both halves: first we prove that `ci-uploader` can write firmware, and then we prove that it cannot write anywhere else.

In the second terminal, switch to the new credentials (paste your generated pair):

```bash
export AWS_ACCESS_KEY_ID=AK...your-generated-key...
export AWS_SECRET_ACCESS_KEY=...your-generated-secret...
```

Inside the granted prefix, the upload is allowed:

```bash
aws --endpoint-url http://localhost:9000 \
  s3 cp fw-1.4.1.tar s3://releases/firmware/widget-3000/fw-1.4.1-rc2.tar
```

```
upload: ./fw-1.4.1.tar to s3://releases/firmware/widget-3000/fw-1.4.1-rc2.tar
```

Outside the granted prefix, the upload is denied:

```bash
aws --endpoint-url http://localhost:9000 \
  s3 cp fw-1.4.1.tar s3://releases/private/fw-1.4.1.tar
```

```
upload failed: ./fw-1.4.1.tar to s3://releases/private/fw-1.4.1.tar An error
occurred (AccessDenied) when calling the CreateMultipartUpload operation:
AccessDenied: Access Denied
```

(The operation name varies with file size, because the CLI uploads a file this big as a multipart upload. The important part is the `AccessDenied`.)

The bucket and the credentials are the same. The only difference is the path, and this path is outside `releases/firmware/*`. The `AccessDenied` shows that least privilege works.

## Step 5: watch the rate limiter

One more defense is already active: the brute-force rate limiter on the admin login. To see it engage, send twelve wrong passwords to the login endpoint:

```bash
for i in $(seq 1 12); do
  curl -s -o /dev/null -w "attempt $i: HTTP %{http_code}\n" \
    -X POST http://localhost:9000/_/api/admin/login \
    -H 'content-type: application/json' \
    --data '{"password":"definitely-wrong"}'
done
```

```
attempt 1: HTTP 401
attempt 2: HTTP 401
...
attempt 10: HTTP 401
attempt 11: HTTP 429
attempt 12: HTTP 429
```

Notice the change after attempt 10. The first failures get a plain `401 Unauthorized`. Then the per-account lockout engages, and every attempt, even one with a *correct* password, gets `429 Too Many Requests`. An attacker gets ten guesses an hour, not ten thousand a second.

Your existing browser session is not affected (you are already signed in), and the lockout expires on its own. To clear it now, restart the container, because the limiter keeps its counters in memory.

## What you built

Take stock of what is now true about this proxy, none of which was true an hour ago:

- The admin password is one that you chose, stored only as a bcrypt hash on your volume.
- Every S3 request must carry a valid SigV4 signature. Anonymous access gets `AccessDenied`.
- CI has its own credential, `ci-uploader`, that can read, write, and list under `releases/firmware/*` and is denied everywhere else.
- Your admin credential survived the switch to IAM as `legacy-admin`.
- The rate limiter blocks password guessing against the admin login after a few attempts.

## Where next

- [Go to production](../how-to/go-to-production.md): TLS, backups, monitoring, and the rest of the checklist between here and real traffic.
- [Set up OAuth/OIDC single sign-on](../how-to/set-up-sso.md): let people log in with Google or any OIDC provider instead of managing their keys by hand.
- [About the security model](../explanation/security-model.md): why the layers (admission, SigV4, IAM, rate limiting) stack the way they do.
