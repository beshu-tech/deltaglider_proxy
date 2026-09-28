# How to back up and restore

This guide shows you how to back up the state of DeltaGlider Proxy (config, IAM, and secrets) and restore it onto the same or a fresh instance.

**Object data is not in any of these backups.** Objects live in your storage backend (`hetzner-fsn1`, `local-disk`, …), so back them up with your storage provider's tools. The proxy itself owns only the config file, the encrypted IAM DB, and the infra secrets.

## Pick the right mechanism

People often confuse these three mechanisms:

| Mechanism | What it is | Use it when |
|---|---|---|
| **Full Backup** (zip from the admin UI or the admin API) | Operator-initiated, point-in-time snapshot: config + IAM + secrets, sha256-verified, atomic restore | Before every upgrade; on a schedule; before risky config changes. This is THE backup. |
| **DB snapshot** (file copy) | Filesystem-level copy of `deltaglider_config.db` (SQLCipher-encrypted SQLite) | You already snapshot volumes (PVC snapshots, ZFS, etc.) and preserve the config DB key alongside |
| **S3 config sync** (`config_sync_bucket`) | Automatic live replication of the encrypted DB across instances | Horizontal scaling or blue-green deployments. See [How to run multiple instances](run-multiple-instances.md). **Not a backup**: a bad mutation propagates to every reader. |

Take a Full Backup in every case. The other two mechanisms add to it and do not replace it.

## Take a Full Backup

A backup is not configuration, so there is no YAML for this task. You take it in the admin UI or with the admin API.

In the admin UI:

1. In the sidebar, open **System → System** (`/_/admin/system`), and scroll to the backup card at the bottom of the page.
2. Click **Download backup**. The browser saves a zip named `dgp-backup-v<version>-<time>.zip`.

   ![The backup card at the bottom of the System page; the box marks the Download backup button.](/_/screenshots/backup-download.webp)

With the admin API, send the same request with an admin session cookie:

```bash
curl -b /tmp/admin.cookies \
  "https://s3.acme.example/_/api/admin/backup" \
  -o dgp-backup-$(date +%Y%m%d-%H%M%S).zip
```

The zip contains four artefacts, each sha256-listed in the manifest:

- `manifest.json`: version, timestamp, checksums
- `config.yaml`: canonical YAML, secrets redacted
- `iam.json`: users, groups, OAuth providers, mapping rules, external identities
- `secrets.json`: **plaintext** infra secrets: bootstrap hash, the bootstrap SigV4 pair, OAuth client_secrets, storage creds, the Slack bot token and webhook header values of event delivery, and the encryption keys (`key` and `legacy_key`) of every backend whose keys are in the config file. A restore puts these keys back, so that objects encrypted before the backup stay readable and new writes stay encrypted. A key that comes from an environment variable, such as `DGP_ENCRYPTION_KEY`, is not in the backup, so set that variable on the new instance before you restore. When the instance uses declarative IAM (`access.iam_mode: declarative`), `secrets.json` also holds the secret access key of every user in `access.iam_users`. The redacted `config.yaml` does not have these keys, and the restore needs them to create the users on a fresh instance, so the restore puts them back into the configuration. A user whose key is a `${env:NAME}` reference keeps the reference, so set that variable on the new instance too

A secret that comes from an environment variable (for example `DGP_SECRET_ACCESS_KEY`, `DGP_BE_AWS_SECRET_ACCESS_KEY` or `DGP_BOOTSTRAP_PASSWORD_HASH`) is not part of the backup. The backup holds only what the config file holds, so a restore never writes an environment value into another instance's config file. The instance you restore onto must get these values from its own environment, so keep them in your secret manager.

`secrets.json` makes the zip a keystore, so treat it like one. Store it encrypted and off the host, in a place that the upgrade or incident you protect against cannot reach. Take a fresh one after any password change, because the zip carries the bootstrap hash.

## Restore a Full Backup

In the admin UI:

1. On **System → System**, click **Restore backup** in the backup card, and select the zip file.

   ![The backup card at the bottom of the System page; the box marks the Restore backup button.](/_/screenshots/backup-restore-open.webp)

2. In the **Restore backup** dialog, select what to restore. The first choice is selected when the dialog opens.
   - **Everything except the admin password**: the config, the backends, the bucket policies, the users, the groups, the OIDC providers and the secrets. This instance keeps its own admin password.
   - **Config only**: the config, the backends and the bucket policies. The users, the groups, the OIDC providers and the admin password stay as they are.
   - **Users and groups only**: the users, the groups and the OIDC providers. The storage settings stay as they are.
   - **Everything, including the admin password**: everything in the backup. This instance takes the admin password of the backup, unless `DGP_BOOTSTRAP_PASSWORD_HASH` sets it.
3. When the choice includes users and groups, select what happens to the users, groups and OIDC providers that exist now:
   - **Replace (point in time)**: after the restore, the instance holds exactly the entries of the backup. Entries that the backup does not hold are deleted.
   - **Merge (keep existing)**: entries that the backup does not hold are kept, and entries that exist now are not overwritten. Only the missing entries are added.
4. Click **Restore**. After a successful restore, the page reloads.

   ![The Restore backup dialog lists what to restore; callout 1 marks the four restore modes, callout 2 marks Replace and Merge for the users and groups that exist now, and callout 3 marks the Restore button.](/_/screenshots/backup-restore-modes.webp)

In declarative IAM mode, the YAML file owns the users and groups, so the dialog offers only **Config only**. To restore users, groups and OIDC providers in that mode, edit `access.iam_*` in the YAML file and apply it.

With the admin API, send the zip in the body of a `POST` request:

```bash
curl -b cookies -X POST \
  -H "Content-Type: application/zip" \
  --data-binary @dgp-backup-20260612-090000.zip \
  "https://s3.acme.example/_/api/admin/backup?mode=preserve-bootstrap&iam=replace"
```

The `mode` parameter selects what to restore, and the `iam` parameter selects `replace` or `merge`:

| Dialog choice | `mode` | Restores the admin password |
|---|---|---|
| Everything except the admin password | `preserve-bootstrap` | no |
| Config only | `config-only` | no |
| Users and groups only | `iam-only` | no |
| Everything, including the admin password | `full` | yes |

Without a `mode` parameter, the API restores in `full` mode, which also restores the admin password. The dialog starts from **Everything except the admin password** instead.

The import is atomic. The proxy unpacks all four parts and verifies their sha256 before any state changes. The proxy then applies the configuration, the secrets and the IAM state in that order, and it writes the OAuth client secrets last. The live OAuth providers pick up these secrets without a restart. Before the first of these steps, it records the running configuration, the admin password and the IAM database. When a later step fails, the proxy puts the recorded state back, in memory and in the config file, so a failed restore leaves the instance as it was before. The proxy remaps `external_identities` through the imported user and provider IDs, so OAuth users keep working. The proxy also accepts a legacy JSON-only body for IAM-only restores from pre-v0.8.4 scripts.

A restore of the config must be saved to the config file, or it would revert at the next restart. When the config file is read-only, for example because a Docker Compose file, the Helm chart or the Kubernetes operator mounts it read-only, the proxy refuses every mode that restores the config, before it changes anything. It answers `409` with the error `config_file_read_only`. On such an instance, restore with **Users and groups only** (`mode=iam-only`), and change the config in the YAML of your deployment.

A restore of users, groups and OIDC providers is point-in-time by default (`iam=replace`). The proxy deletes every user, group, OIDC provider, mapping rule and external identity that exists now, and then writes the rows from the backup, in one database transaction. So after the restore, the instance holds exactly the backup's users and groups: a user that someone created after the backup is gone, and a user that someone changed after the backup has the backup's settings again. If any row fails to write, the transaction rolls back and nothing changes. The proxy keeps the backup's user IDs, so OAuth logins keep working, and it ends the OAuth sessions of users that the restore deletes. A merge (`iam=merge`) adds the users and groups that the instance does not have and does not change the ones that it has.

Only the `full` mode restores the admin password: when the zip's `secrets.json` carries a bootstrap password hash that differs from the running instance's, the instance adopts it, and the backup's admin password works from then on. The hash does not encrypt anything, so this is safe. There is one exception: when `DGP_BOOTSTRAP_PASSWORD_HASH` is set, it sets the hash at every start, so the restore keeps the running password and logs a warning. The modes `preserve-bootstrap`, `config-only` and `iam-only` keep this instance's admin password.

The zip carries the IAM state as plain JSON (`iam.json`), so a restore onto a **fresh instance** does not need the old instance's config DB key: the fresh instance writes the imported users into its own database, under its own key. The zip's `secrets.json` holds the bootstrap password hash only when the config file held it; a hash that came from the environment is not in the backup.

## Snapshot the DB file

`deltaglider_config.db` is safe to copy at the file level, because it is encrypted at rest. The snapshot is useful only if you **keep the config DB key with it**: without the key, nobody can open the file. The key is the value of `DGP_CONFIG_DB_KEY` or, when that variable is unset, the content of the key file `deltaglider_config.db.key` next to the DB. Store the key in your secret manager, and the snapshot restores onto any instance that starts with the same key. A snapshot from a release that came before the config DB key is encrypted with the bootstrap password hash instead; an instance that starts with that hash re-encrypts it with its config DB key on the first start.

## The xattr warning (filesystem backend)

If you back up a **filesystem backend's data directory** with file copies, your tool must preserve extended attributes, because the per-object metadata lives in the `user.dg.metadata` xattr on the inode of each file. A tool that copies contents but drops xattrs (older `rsync` without `-X`, many archive tools) produces a restore where encrypted objects fail to read and delta objects lose their metadata.

```bash
rsync -aX /var/lib/deltaglider_proxy/data/ /backup/dgp-data/   # -X = preserve xattrs
```

The usual symptom after a bad restore is that reads return 500 with "xattrs may have been stripped during backup/restore". See [Troubleshooting](troubleshooting.md).

## Verify

After a restore:

```bash
# Health
curl -s https://s3.acme.example/_/health

# IAM users came back
curl -b cookies https://s3.acme.example/_/api/admin/users | jq '.[] | .name'
# expect: ci-uploader, backup-bot, dana, ...

# A known object still reads byte-identical
aws --endpoint-url https://s3.acme.example s3 cp s3://releases/known-file ./out
sha256sum out   # matches the recorded checksum
```

Then log in to the admin UI. After a restore in `full` mode, the admin password of the backup works. After any other mode, the admin password of this instance still works.

## Related

- [How to upgrade the proxy](upgrade.md): backup is step 1 of every upgrade
- [How to run multiple instances (HA)](run-multiple-instances.md): what config sync is for
- [Admin API reference](../reference/admin-api.md): backup endpoint details
- [Troubleshooting](troubleshooting.md): SQLCipher and xattr failure symptoms
