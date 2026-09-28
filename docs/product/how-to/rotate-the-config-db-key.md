# How to rotate the config DB key

This guide shows you how to move the encrypted config database (`deltaglider_config.db`) to a new key, on one instance or on a fleet of instances that share a sync bucket. The key and its sources are described in [the authentication reference](../reference/authentication.md#config-database-key).

The key exists only in the environment or in the key file next to the database. The admin UI and the YAML file cannot set it, so every step here sets environment variables and restarts the proxy.

The rotation works because the proxy tries more than one key when it opens the database. It tries `DGP_CONFIG_DB_KEY` first and `DGP_CONFIG_DB_KEY_PREVIOUS` after it. When the database opens only with the previous key, the proxy re-encrypts it with the new key. It re-encrypts a copy, checks that the copy opens with the new key, and only then replaces the original, so a failed step never damages the database. The sync merge base next to the database (`deltaglider_config.db.sync-base`) moves to the new key in the same step.

## Rotate the key of one instance

1. Generate a new key:

   ```bash
   openssl rand -hex 32
   ```

2. Set `DGP_CONFIG_DB_KEY_PREVIOUS` to the current key. That is the old value of `DGP_CONFIG_DB_KEY`, or the content of `deltaglider_config.db.key` when the variable was unset.
3. Set `DGP_CONFIG_DB_KEY` to the new key, and restart the proxy.
4. Check the log for the line `re-encrypted with DGP_CONFIG_DB_KEY`.
5. Remove `DGP_CONFIG_DB_KEY_PREVIOUS`, restart the proxy again, and store the new key with your database backups.

## Rotate the key of a fleet with a sync bucket

The synced database, and the local database of every instance, move to the new key during one rolling restart:

1. Generate a new key (`openssl rand -hex 32`).
2. On every instance, set `DGP_CONFIG_DB_KEY_PREVIOUS` to the current key and `DGP_CONFIG_DB_KEY` to the new key.
3. Restart the instances one at a time. Each instance re-encrypts its local database with the new key. Because its database changed key, it also uploads its database to the sync bucket at start, so the synced copy moves to the new key too. An instance that downloads a synced copy that opens only with the previous key also uploads its merged database under the new key. While the rollout runs, the instances that already run with the new key still read a synced copy under the previous key.
4. When every instance runs with the new key, remove `DGP_CONFIG_DB_KEY_PREVIOUS` everywhere and restart again. An instance that later finds a synced copy under the old key logs an error that names `DGP_CONFIG_DB_KEY` and does not merge it.

Instances that have not restarted yet cannot read uploads under the new key, so avoid IAM changes during step 3.

## Move a fleet from a release before the config DB key

Releases before the config DB key encrypted the database with the bootstrap password hash. To move such a fleet:

1. Set the same new `DGP_CONFIG_DB_KEY` on every instance.
2. Also set `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true` on every instance for the rollout. On the first start, each instance re-encrypts its local database with the new key. The variable lets it also accept a synced database under the old hash. Without the variable, an instance refuses such a synced database, because the hash is in configuration files and backups, so it cannot protect a database that other instances trust.
3. Restart all instances.
4. When the rollout is complete, remove `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC` and restart again.

Until the last instance runs the new release, the old instances cannot read the uploads of the new ones, so avoid IAM changes during the rollout.

## Verify

1. Check the log of every instance for the line `re-encrypted with DGP_CONFIG_DB_KEY` after the first restart.
2. After the last restart, without `DGP_CONFIG_DB_KEY_PREVIOUS`, check that the admin API still lists the users:

   ```bash
   curl -b cookies https://s3.acme.example/_/api/admin/users | jq '.[] | .name'
   ```

3. With a sync bucket, check that every instance syncs:

   ```bash
   curl -b cookies https://s3.acme.example/_/api/admin/config/sync | jq .healthy
   ```

   The answer is `true`. An instance whose key does not open the synced copy reports an error that names `DGP_CONFIG_DB_KEY`.

## Related

- [Authentication reference](../reference/authentication.md#config-database-key): the key sources and what happens when no key opens the database.
- [How to run multiple instances (HA)](run-multiple-instances.md): the sync bucket and the shared key.
- [How to back up and restore](back-up-and-restore.md): back up the key with the database.
