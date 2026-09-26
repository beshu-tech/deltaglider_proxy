# How to back up and restore

This guide shows you how to back up DeltaGlider Proxy's state — config, IAM, and secrets — and restore it onto the same or a fresh instance.

One thing first: **object data is not in any of these backups.** Objects live in your storage backend (`hetzner-fsn1`, `local-disk`, …); back that up with your storage provider's tools. What the proxy itself owns is the config file, the encrypted IAM DB, and the infra secrets.

## Pick the right mechanism

Three mechanisms, routinely confused:

| Mechanism | What it is | Use it when |
|---|---|---|
| **Full Backup** (zip via admin API) | Operator-initiated, point-in-time snapshot: config + IAM + secrets, sha256-verified, atomic restore | Before every upgrade; on a schedule; before risky config changes. This is THE backup. |
| **DB snapshot** (file copy) | Filesystem-level copy of `deltaglider_config.db` (SQLCipher-encrypted SQLite) | You already snapshot volumes (PVC snapshots, ZFS, etc.) and preserve the bootstrap password alongside |
| **S3 config sync** (`config_sync_bucket`) | Automatic live replication of the encrypted DB across instances | Horizontal scaling / blue-green — see [How to run multiple instances](run-multiple-instances.md). **Not a backup**: a bad mutation propagates to every reader. |

You want Full Backup regardless; the other two are complements, not substitutes.

## Take a Full Backup

![Full Backup export and restore](/_/screenshots/backup-restore.jpg)

From the admin UI: **Settings → System → Backup → Export**. Or via API:

```bash
curl -b /tmp/admin.cookies \
  "https://s3.acme.example/_/api/admin/backup" \
  -o dgp-backup-$(date +%Y%m%d-%H%M%S).zip
```

The zip contains four artefacts, each sha256-listed in the manifest:

- `manifest.json` — version, timestamp, checksums
- `config.yaml` — canonical YAML, secrets redacted
- `iam.json` — users, groups, OAuth providers, mapping rules, external identities
- `secrets.json` — **plaintext** infra secrets: bootstrap hash, OAuth client_secrets, storage creds, and the encryption keys (`key` and `legacy_key`) of every backend whose keys are in the config file. A restore puts these keys back, so that objects encrypted before the backup stay readable and new writes stay encrypted. A key that comes from an environment variable, such as `DGP_ENCRYPTION_KEY`, is not in the backup, so set that variable on the new instance before you restore

A secret that comes from an environment variable (for example `DGP_SECRET_ACCESS_KEY`, `DGP_BE_AWS_SECRET_ACCESS_KEY` or `DGP_BOOTSTRAP_PASSWORD_HASH`) is not part of the backup. The backup holds only what the config file holds, so a restore never writes an environment value into another instance's config file. The instance you restore onto must get these values from its own environment, so keep them in your secret manager.

`secrets.json` makes the zip a keystore — treat it like one. Store it encrypted, off the host, somewhere the upgrade or incident you're protecting against can't reach. Take a fresh one after any password change: the zip carries both the bootstrap hash and the encrypted DB, so a fresh instance can be reconstituted from it alone.

## Restore a Full Backup

```bash
curl -b cookies -X POST \
  -H "Content-Type: application/zip" \
  --data-binary @dgp-backup-20260612-090000.zip \
  https://s3.acme.example/_/api/admin/backup
```

The import is atomic: all four parts are unpacked and sha256-verified before any state changes. `external_identities` are remapped through the imported user and provider IDs, so OAuth users keep working. A legacy JSON-only body is still accepted for IAM-only restores from pre-v0.8.4 scripts.

A full restore (the default mode, and `mode=config-only`) also restores the admin password: when the zip's `secrets.json` carries a bootstrap password hash that differs from the running instance's, the instance adopts it, and the backup's admin password works from then on. The hash does not encrypt anything, so this is safe. There is one exception: when `DGP_BOOTSTRAP_PASSWORD_HASH` is set, it sets the hash at every start, so the restore keeps the running password and logs a warning. Use `mode=preserve-bootstrap` to restore everything except the admin password, or `mode=iam-only` to keep this instance's admin password and storage configuration.

The zip carries the IAM state as plain JSON (`iam.json`), so a restore onto a **fresh instance** does not need the old instance's config DB key: the fresh instance writes the imported users into its own database, under its own key. The zip's `secrets.json` holds the bootstrap password hash only when the config file held it; a hash that came from the environment is not in the backup.

## Snapshot the DB file

`deltaglider_config.db` is safe to copy at the file level — it's encrypted at rest. The snapshot is only useful if the **config DB key is preserved with it**: without the key, nobody can open the file. The key is the value of `DGP_CONFIG_DB_KEY` or, when that variable is unset, the content of the key file `deltaglider_config.db.key` next to the DB. Store the key in your secret manager, and the snapshot restores onto any instance that starts with the same key. A snapshot from a release before the config DB key is encrypted with the bootstrap password hash instead; an instance that starts with that hash re-encrypts it with its config DB key on the first start.

## The xattr warning (filesystem backend)

If you back up a **filesystem backend's data directory** with file copies, your tool must preserve extended attributes — per-object metadata lives in the `user.dg.metadata` xattr on each file's inode. A tool that copies contents but drops xattrs (older `rsync` without `-X`, many archive tools) produces a restore where encrypted objects fail to read and delta objects lose their metadata.

```bash
rsync -aX /var/lib/deltaglider_proxy/data/ /backup/dgp-data/   # -X = preserve xattrs
```

The classic symptom after a bad restore: reads return 500 with "xattrs may have been stripped during backup/restore" — see [Troubleshooting](troubleshooting.md).

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

And log in to the admin UI with the bootstrap password — if that works, the DB and hash are in agreement.

## Related

- [How to upgrade the proxy](upgrade.md) — backup is step 1 of every upgrade
- [How to run multiple instances (HA)](run-multiple-instances.md) — what config sync is actually for
- [Admin API reference](../reference/admin-api.md) — backup endpoint details
- [Troubleshooting](troubleshooting.md) — SQLCipher and xattr failure symptoms
