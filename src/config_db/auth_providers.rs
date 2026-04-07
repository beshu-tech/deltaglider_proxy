//! CRUD operations for external auth providers, group mapping rules, and external identities.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{ConfigDb, ConfigDbError};

// ── Auth Provider types ──

/// Configuration for an external authentication provider (stored in ConfigDb).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderConfig {
    pub id: i64,
    pub name: String,
    pub provider_type: String,
    pub enabled: bool,
    pub priority: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Masked in API responses — never returned in full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: String,
    /// Provider-specific JSON config (e.g., allowed_domains, email_verified_required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_config: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_scopes() -> String {
    "openid email profile".to_string()
}

/// Request to create a new auth provider.
#[derive(Debug, Deserialize)]
pub struct CreateAuthProviderRequest {
    pub name: String,
    pub provider_type: String,
    #[serde(default = "crate::types::default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    pub display_name: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub issuer_url: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: String,
    pub extra_config: Option<serde_json::Value>,
}

/// Request to update an existing auth provider.
#[derive(Debug, Deserialize)]
pub struct UpdateAuthProviderRequest {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub enabled: Option<bool>,
    pub priority: Option<i64>,
    pub display_name: Option<String>,
    pub client_id: Option<String>,
    /// If provided, replaces the secret. If None, keeps existing.
    pub client_secret: Option<String>,
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
    pub extra_config: Option<serde_json::Value>,
}

// ── Group Mapping Rule types ──

/// A rule that maps an external identity attribute to a local group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMappingRule {
    pub id: i64,
    /// Provider ID this rule applies to (None = all providers).
    pub provider_id: Option<i64>,
    pub priority: i64,
    /// Match type: "email_exact", "email_domain", "email_regex", "claim_value".
    pub match_type: String,
    /// Field to match on: "email", "hd", "groups", or any custom claim key.
    pub match_field: String,
    /// Value to match against.
    pub match_value: String,
    /// Local group ID to assign on match.
    pub group_id: i64,
    pub created_at: String,
}

/// Request to create a mapping rule.
#[derive(Debug, Deserialize)]
pub struct CreateMappingRuleRequest {
    pub provider_id: Option<i64>,
    #[serde(default)]
    pub priority: i64,
    pub match_type: String,
    #[serde(default = "default_email")]
    pub match_field: String,
    pub match_value: String,
    pub group_id: i64,
}

fn default_email() -> String {
    "email".to_string()
}

/// Request to update a mapping rule.
#[derive(Debug, Deserialize)]
pub struct UpdateMappingRuleRequest {
    pub provider_id: Option<Option<i64>>,
    pub priority: Option<i64>,
    pub match_type: Option<String>,
    pub match_field: Option<String>,
    pub match_value: Option<String>,
    pub group_id: Option<i64>,
}

// ── External Identity types ──

/// An external identity linked to a local IAM user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalIdentity {
    pub id: i64,
    pub user_id: i64,
    pub provider_id: i64,
    pub external_sub: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub last_login: Option<String>,
    pub raw_claims: Option<serde_json::Value>,
    pub created_at: String,
}

// ── ConfigDb CRUD implementations ──

impl ConfigDb {
    // ── Auth Providers ──

    /// Map a row from the auth_providers table to an AuthProviderConfig.
    fn auth_provider_from_row(row: &rusqlite::Row) -> rusqlite::Result<AuthProviderConfig> {
        let extra_config: Option<serde_json::Value> = row
            .get::<_, Option<String>>(10)?
            .and_then(|s| serde_json::from_str(&s).ok());
        Ok(AuthProviderConfig {
            id: row.get(0)?,
            name: row.get(1)?,
            provider_type: row.get(2)?,
            enabled: row.get::<_, i32>(3)? != 0,
            priority: row.get(4)?,
            display_name: row.get(5)?,
            client_id: row.get(6)?,
            client_secret: row.get(7)?,
            issuer_url: row.get(8)?,
            scopes: row.get::<_, String>(9).unwrap_or_else(|_| default_scopes()),
            extra_config,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    }

    /// Load all auth providers.
    pub fn load_auth_providers(&self) -> Result<Vec<AuthProviderConfig>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, provider_type, enabled, priority, display_name, \
             client_id, client_secret, issuer_url, scopes, extra_config, \
             created_at, updated_at \
             FROM auth_providers ORDER BY priority DESC, id ASC",
        )?;
        let providers = stmt
            .query_map([], Self::auth_provider_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(providers)
    }

    /// Get a single auth provider by ID.
    pub fn get_auth_provider(&self, id: i64) -> Result<AuthProviderConfig, ConfigDbError> {
        self.conn
            .query_row(
                "SELECT id, name, provider_type, enabled, priority, display_name, \
                 client_id, client_secret, issuer_url, scopes, extra_config, \
                 created_at, updated_at \
                 FROM auth_providers WHERE id = ?1",
                params![id],
                Self::auth_provider_from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ConfigDbError::NotFound(format!("Auth provider ID {}", id))
                }
                other => ConfigDbError::Sqlite(other),
            })
    }

    /// Get a single auth provider by name.
    pub fn get_auth_provider_by_name(
        &self,
        name: &str,
    ) -> Result<Option<AuthProviderConfig>, ConfigDbError> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM auth_providers WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => Ok(Some(self.get_auth_provider(id)?)),
            None => Ok(None),
        }
    }

    /// Create a new auth provider.
    pub fn create_auth_provider(
        &self,
        req: &CreateAuthProviderRequest,
    ) -> Result<AuthProviderConfig, ConfigDbError> {
        let extra_json: Option<String> = req
            .extra_config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        self.conn.execute(
            "INSERT INTO auth_providers (name, provider_type, enabled, priority, display_name, \
             client_id, client_secret, issuer_url, scopes, extra_config) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                req.name,
                req.provider_type,
                req.enabled as i32,
                req.priority,
                req.display_name,
                req.client_id,
                req.client_secret,
                req.issuer_url,
                req.scopes,
                extra_json,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get_auth_provider(id)
    }

    /// Update an existing auth provider.
    pub fn update_auth_provider(
        &self,
        id: i64,
        req: &UpdateAuthProviderRequest,
    ) -> Result<AuthProviderConfig, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(ref name) = req.name {
            tx.execute(
                "UPDATE auth_providers SET name = ?1 WHERE id = ?2",
                params![name, id],
            )?;
        }
        if let Some(ref provider_type) = req.provider_type {
            tx.execute(
                "UPDATE auth_providers SET provider_type = ?1 WHERE id = ?2",
                params![provider_type, id],
            )?;
        }
        if let Some(enabled) = req.enabled {
            tx.execute(
                "UPDATE auth_providers SET enabled = ?1 WHERE id = ?2",
                params![enabled as i32, id],
            )?;
        }
        if let Some(priority) = req.priority {
            tx.execute(
                "UPDATE auth_providers SET priority = ?1 WHERE id = ?2",
                params![priority, id],
            )?;
        }
        if let Some(ref display_name) = req.display_name {
            tx.execute(
                "UPDATE auth_providers SET display_name = ?1 WHERE id = ?2",
                params![display_name, id],
            )?;
        }
        if let Some(ref client_id) = req.client_id {
            tx.execute(
                "UPDATE auth_providers SET client_id = ?1 WHERE id = ?2",
                params![client_id, id],
            )?;
        }
        if let Some(ref client_secret) = req.client_secret {
            tx.execute(
                "UPDATE auth_providers SET client_secret = ?1 WHERE id = ?2",
                params![client_secret, id],
            )?;
        }
        if let Some(ref issuer_url) = req.issuer_url {
            tx.execute(
                "UPDATE auth_providers SET issuer_url = ?1 WHERE id = ?2",
                params![issuer_url, id],
            )?;
        }
        if let Some(ref scopes) = req.scopes {
            tx.execute(
                "UPDATE auth_providers SET scopes = ?1 WHERE id = ?2",
                params![scopes, id],
            )?;
        }
        if let Some(ref extra_config) = req.extra_config {
            let json = serde_json::to_string(extra_config).unwrap_or_default();
            tx.execute(
                "UPDATE auth_providers SET extra_config = ?1 WHERE id = ?2",
                params![json, id],
            )?;
        }
        tx.execute(
            "UPDATE auth_providers SET updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        self.get_auth_provider(id)
    }

    /// Delete an auth provider by ID.
    pub fn delete_auth_provider(&self, id: i64) -> Result<(), ConfigDbError> {
        let rows = self
            .conn
            .execute("DELETE FROM auth_providers WHERE id = ?1", params![id])?;
        if rows == 0 {
            return Err(ConfigDbError::NotFound(format!("Auth provider ID {}", id)));
        }
        Ok(())
    }

    // ── Group Mapping Rules ──

    /// Map a row from the group_mapping_rules table to a GroupMappingRule.
    fn group_mapping_rule_from_row(row: &rusqlite::Row) -> rusqlite::Result<GroupMappingRule> {
        Ok(GroupMappingRule {
            id: row.get(0)?,
            provider_id: row.get(1)?,
            priority: row.get(2)?,
            match_type: row.get(3)?,
            match_field: row.get(4)?,
            match_value: row.get(5)?,
            group_id: row.get(6)?,
            created_at: row.get(7)?,
        })
    }

    /// Load all group mapping rules.
    pub fn load_group_mapping_rules(&self) -> Result<Vec<GroupMappingRule>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider_id, priority, match_type, match_field, match_value, \
             group_id, created_at \
             FROM group_mapping_rules ORDER BY priority DESC, id ASC",
        )?;
        let rules = stmt
            .query_map([], Self::group_mapping_rule_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rules)
    }

    /// Create a new group mapping rule.
    pub fn create_group_mapping_rule(
        &self,
        req: &CreateMappingRuleRequest,
    ) -> Result<GroupMappingRule, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO group_mapping_rules (provider_id, priority, match_type, match_field, \
             match_value, group_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                req.provider_id,
                req.priority,
                req.match_type,
                req.match_field,
                req.match_value,
                req.group_id,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get_group_mapping_rule(id)
    }

    /// Get a single group mapping rule by ID.
    fn get_group_mapping_rule(&self, id: i64) -> Result<GroupMappingRule, ConfigDbError> {
        self.conn
            .query_row(
                "SELECT id, provider_id, priority, match_type, match_field, match_value, \
                 group_id, created_at \
                 FROM group_mapping_rules WHERE id = ?1",
                params![id],
                Self::group_mapping_rule_from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ConfigDbError::NotFound(format!("Mapping rule ID {}", id))
                }
                other => ConfigDbError::Sqlite(other),
            })
    }

    /// Update a group mapping rule.
    pub fn update_group_mapping_rule(
        &self,
        id: i64,
        req: &UpdateMappingRuleRequest,
    ) -> Result<GroupMappingRule, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(ref provider_id) = req.provider_id {
            tx.execute(
                "UPDATE group_mapping_rules SET provider_id = ?1 WHERE id = ?2",
                params![provider_id, id],
            )?;
        }
        if let Some(priority) = req.priority {
            tx.execute(
                "UPDATE group_mapping_rules SET priority = ?1 WHERE id = ?2",
                params![priority, id],
            )?;
        }
        if let Some(ref match_type) = req.match_type {
            tx.execute(
                "UPDATE group_mapping_rules SET match_type = ?1 WHERE id = ?2",
                params![match_type, id],
            )?;
        }
        if let Some(ref match_field) = req.match_field {
            tx.execute(
                "UPDATE group_mapping_rules SET match_field = ?1 WHERE id = ?2",
                params![match_field, id],
            )?;
        }
        if let Some(ref match_value) = req.match_value {
            tx.execute(
                "UPDATE group_mapping_rules SET match_value = ?1 WHERE id = ?2",
                params![match_value, id],
            )?;
        }
        if let Some(group_id) = req.group_id {
            tx.execute(
                "UPDATE group_mapping_rules SET group_id = ?1 WHERE id = ?2",
                params![group_id, id],
            )?;
        }
        tx.commit()?;
        self.get_group_mapping_rule(id)
    }

    /// Delete a group mapping rule.
    pub fn delete_group_mapping_rule(&self, id: i64) -> Result<(), ConfigDbError> {
        let rows = self
            .conn
            .execute("DELETE FROM group_mapping_rules WHERE id = ?1", params![id])?;
        if rows == 0 {
            return Err(ConfigDbError::NotFound(format!("Mapping rule ID {}", id)));
        }
        Ok(())
    }

    // ── External Identities ──

    /// List all external identities.
    pub fn list_external_identities(&self) -> Result<Vec<ExternalIdentity>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, provider_id, external_sub, email, display_name, \
             last_login, raw_claims, created_at \
             FROM external_identities ORDER BY id ASC",
        )?;
        let identities = stmt
            .query_map([], Self::external_identity_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(identities)
    }

    /// Find an external identity by provider + subject (stable unique ID from IdP).
    pub fn find_external_identity(
        &self,
        provider_id: i64,
        external_sub: &str,
    ) -> Result<Option<ExternalIdentity>, ConfigDbError> {
        let result = self
            .conn
            .query_row(
                "SELECT id, user_id, provider_id, external_sub, email, display_name, \
                 last_login, raw_claims, created_at \
                 FROM external_identities WHERE provider_id = ?1 AND external_sub = ?2",
                params![provider_id, external_sub],
                Self::external_identity_from_row,
            )
            .optional()?;
        Ok(result)
    }

    /// Create a new external identity record.
    pub fn create_external_identity(
        &self,
        user_id: i64,
        provider_id: i64,
        external_sub: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        raw_claims: Option<&serde_json::Value>,
    ) -> Result<ExternalIdentity, ConfigDbError> {
        let claims_json: Option<String> =
            raw_claims.map(|v| serde_json::to_string(v).unwrap_or_default());
        self.conn.execute(
            "INSERT INTO external_identities (user_id, provider_id, external_sub, email, \
             display_name, last_login, raw_claims) \
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), ?6)",
            params![
                user_id,
                provider_id,
                external_sub,
                email,
                display_name,
                claims_json
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get_external_identity(id)
    }

    /// Update an external identity (on returning login).
    pub fn update_external_identity(
        &self,
        id: i64,
        email: Option<&str>,
        display_name: Option<&str>,
        raw_claims: Option<&serde_json::Value>,
    ) -> Result<ExternalIdentity, ConfigDbError> {
        let claims_json: Option<String> =
            raw_claims.map(|v| serde_json::to_string(v).unwrap_or_default());
        self.conn.execute(
            "UPDATE external_identities SET email = ?1, display_name = ?2, \
             last_login = datetime('now'), raw_claims = ?3 WHERE id = ?4",
            params![email, display_name, claims_json, id],
        )?;
        self.get_external_identity(id)
    }

    /// Get external identities for a specific user.
    pub fn get_external_identities_for_user(
        &self,
        user_id: i64,
    ) -> Result<Vec<ExternalIdentity>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, provider_id, external_sub, email, display_name, \
             last_login, raw_claims, created_at \
             FROM external_identities WHERE user_id = ?1",
        )?;
        let identities = stmt
            .query_map(params![user_id], Self::external_identity_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(identities)
    }

    fn get_external_identity(&self, id: i64) -> Result<ExternalIdentity, ConfigDbError> {
        self.conn
            .query_row(
                "SELECT id, user_id, provider_id, external_sub, email, display_name, \
                 last_login, raw_claims, created_at \
                 FROM external_identities WHERE id = ?1",
                params![id],
                Self::external_identity_from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ConfigDbError::NotFound(format!("External identity ID {}", id))
                }
                other => ConfigDbError::Sqlite(other),
            })
    }

    fn external_identity_from_row(row: &rusqlite::Row) -> rusqlite::Result<ExternalIdentity> {
        let raw_claims: Option<serde_json::Value> = row
            .get::<_, Option<String>>(7)?
            .and_then(|s| serde_json::from_str(&s).ok());
        Ok(ExternalIdentity {
            id: row.get(0)?,
            user_id: row.get(1)?,
            provider_id: row.get(2)?,
            external_sub: row.get(3)?,
            email: row.get(4)?,
            display_name: row.get(5)?,
            last_login: row.get(6)?,
            raw_claims,
            created_at: row.get(8)?,
        })
    }

    /// Create a user with auth_source set to "external" (auto-provisioned via OAuth).
    pub fn create_external_user(
        &self,
        name: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Result<crate::iam::IamUser, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO users (name, access_key_id, secret_access_key, enabled, auth_source) \
             VALUES (?1, ?2, ?3, 1, 'external')",
            params![name, access_key_id, secret_access_key],
        )?;
        let user_id = tx.last_insert_rowid();
        tx.commit()?;
        self.get_user_by_id(user_id)
    }

    /// Set group memberships for a user, replacing all existing memberships.
    /// Used by the OAuth callback to reconcile groups after mapping evaluation.
    pub fn set_user_group_memberships(
        &self,
        user_id: i64,
        group_ids: &[i64],
    ) -> Result<(), ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM group_members WHERE user_id = ?1",
            params![user_id],
        )?;
        for &gid in group_ids {
            tx.execute(
                "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
                params![gid, user_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_db::ConfigDb;

    #[test]
    fn test_auth_provider_crud() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        // Create
        let mut req = make_provider_req("google-corp");
        req.priority = 10;
        req.display_name = Some("Google Workspace".into());
        let provider = db.create_auth_provider(&req).unwrap();
        assert_eq!(provider.name, "google-corp");
        assert_eq!(provider.provider_type, "oidc");
        assert!(provider.enabled);
        assert_eq!(provider.display_name.as_deref(), Some("Google Workspace"));

        // Load all
        let providers = db.load_auth_providers().unwrap();
        assert_eq!(providers.len(), 1);

        // Get by name
        let found = db
            .get_auth_provider_by_name("google-corp")
            .unwrap()
            .unwrap();
        assert_eq!(found.id, provider.id);

        // Update
        let update = UpdateAuthProviderRequest {
            name: None,
            provider_type: None,
            enabled: Some(false),
            priority: None,
            display_name: Some("Google (disabled)".into()),
            client_id: None,
            client_secret: None,
            issuer_url: None,
            scopes: None,
            extra_config: None,
        };
        let updated = db.update_auth_provider(provider.id, &update).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.display_name.as_deref(), Some("Google (disabled)"));

        // Delete
        db.delete_auth_provider(provider.id).unwrap();
        let providers = db.load_auth_providers().unwrap();
        assert!(providers.is_empty());
    }

    #[test]
    fn test_group_mapping_rule_crud() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        // Need a group first
        let group = db.create_group("employees", "All employees", &[]).unwrap();

        // Create rule
        let req = CreateMappingRuleRequest {
            provider_id: None,
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "company.com".into(),
            group_id: group.id,
        };
        let rule = db.create_group_mapping_rule(&req).unwrap();
        assert_eq!(rule.match_type, "email_domain");
        assert_eq!(rule.match_value, "company.com");
        assert_eq!(rule.group_id, group.id);
        assert!(rule.provider_id.is_none());

        // Load all
        let rules = db.load_group_mapping_rules().unwrap();
        assert_eq!(rules.len(), 1);

        // Update
        let update = UpdateMappingRuleRequest {
            provider_id: None,
            priority: Some(20),
            match_type: None,
            match_field: None,
            match_value: Some("newcorp.com".into()),
            group_id: None,
        };
        let updated = db.update_group_mapping_rule(rule.id, &update).unwrap();
        assert_eq!(updated.priority, 20);
        assert_eq!(updated.match_value, "newcorp.com");

        // Delete
        db.delete_group_mapping_rule(rule.id).unwrap();
        let rules = db.load_group_mapping_rules().unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_mapping_rule_cascades_on_group_delete() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let group = db.create_group("temp-group", "", &[]).unwrap();

        let req = CreateMappingRuleRequest {
            provider_id: None,
            priority: 0,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "example.com".into(),
            group_id: group.id,
        };
        db.create_group_mapping_rule(&req).unwrap();

        // Delete group — rule should cascade
        db.delete_group(group.id).unwrap();
        let rules = db.load_group_mapping_rules().unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_external_identity_crud() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        // Create a local user and provider first
        let user = db
            .create_user("alice", "AKALICE1", "secret", true, &[])
            .unwrap();

        let provider = db
            .create_auth_provider(&make_provider_req("google"))
            .unwrap();

        // Create identity
        let claims = serde_json::json!({"email": "alice@company.com", "sub": "google-123"});
        let identity = db
            .create_external_identity(
                user.id,
                provider.id,
                "google-123",
                Some("alice@company.com"),
                Some("Alice"),
                Some(&claims),
            )
            .unwrap();
        assert_eq!(identity.external_sub, "google-123");
        assert_eq!(identity.email.as_deref(), Some("alice@company.com"));

        // Find by provider + sub
        let found = db
            .find_external_identity(provider.id, "google-123")
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().user_id, user.id);

        // Not found
        let missing = db
            .find_external_identity(provider.id, "unknown-sub")
            .unwrap();
        assert!(missing.is_none());

        // Update
        let updated = db
            .update_external_identity(
                identity.id,
                Some("alice@newcorp.com"),
                Some("Alice N"),
                None,
            )
            .unwrap();
        assert_eq!(updated.email.as_deref(), Some("alice@newcorp.com"));

        // List all
        let all = db.list_external_identities().unwrap();
        assert_eq!(all.len(), 1);

        // Get for user
        let for_user = db.get_external_identities_for_user(user.id).unwrap();
        assert_eq!(for_user.len(), 1);
    }

    #[test]
    fn test_external_identity_cascades_on_user_delete() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let user = db
            .create_user("bob", "AKBOB001", "secret", true, &[])
            .unwrap();
        let provider = db.create_auth_provider(&make_provider_req("okta")).unwrap();

        db.create_external_identity(user.id, provider.id, "okta-456", None, None, None)
            .unwrap();

        // Delete user — identity should cascade
        db.delete_user(user.id).unwrap();
        let identities = db.list_external_identities().unwrap();
        assert!(identities.is_empty());
    }

    #[test]
    fn test_create_external_user() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let user = db
            .create_external_user("alice@company.com", "AKEXT001", "autogen-secret")
            .unwrap();
        assert_eq!(user.auth_source, "external");
        assert_eq!(user.name, "alice@company.com");
        assert!(user.enabled);
    }

    // ── Edge case tests ──

    fn make_provider_req(name: &str) -> CreateAuthProviderRequest {
        CreateAuthProviderRequest {
            name: name.into(),
            provider_type: "oidc".into(),
            enabled: true,
            priority: 0,
            display_name: None,
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            issuer_url: Some("https://issuer.example.com".into()),
            scopes: "openid".into(),
            extra_config: None,
        }
    }

    #[test]
    fn test_duplicate_provider_name_rejected() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        db.create_auth_provider(&make_provider_req("google"))
            .unwrap();
        let result = db.create_auth_provider(&make_provider_req("google"));
        assert!(result.is_err(), "Duplicate provider name should fail");
    }

    #[test]
    fn test_delete_nonexistent_provider() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let result = db.delete_auth_provider(99999);
        assert!(
            matches!(result, Err(crate::config_db::ConfigDbError::NotFound(_))),
            "Delete nonexistent provider should return NotFound"
        );
    }

    #[test]
    fn test_delete_nonexistent_mapping_rule() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let result = db.delete_group_mapping_rule(99999);
        assert!(matches!(
            result,
            Err(crate::config_db::ConfigDbError::NotFound(_))
        ));
    }

    #[test]
    fn test_provider_cascade_to_mapping_rules() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let provider = db.create_auth_provider(&make_provider_req("prov")).unwrap();
        let group = db.create_group("g", "", &[]).unwrap();

        db.create_group_mapping_rule(&CreateMappingRuleRequest {
            provider_id: Some(provider.id),
            priority: 0,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "co.com".into(),
            group_id: group.id,
        })
        .unwrap();

        // Delete provider — rule scoped to it should be cascade-deleted
        db.delete_auth_provider(provider.id).unwrap();
        let rules = db.load_group_mapping_rules().unwrap();
        assert!(
            rules.is_empty(),
            "Mapping rules should cascade on provider delete"
        );
    }

    #[test]
    fn test_provider_cascade_to_external_identities() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let provider = db.create_auth_provider(&make_provider_req("prov")).unwrap();
        let user = db.create_user("u", "AK1", "SK1", true, &[]).unwrap();

        db.create_external_identity(user.id, provider.id, "sub-1", None, None, None)
            .unwrap();

        db.delete_auth_provider(provider.id).unwrap();
        let ids = db.list_external_identities().unwrap();
        assert!(
            ids.is_empty(),
            "External identities should cascade on provider delete"
        );
    }

    #[test]
    fn test_external_identity_unique_constraint() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let provider = db.create_auth_provider(&make_provider_req("prov")).unwrap();
        let user = db.create_user("u", "AK1", "SK1", true, &[]).unwrap();

        db.create_external_identity(user.id, provider.id, "sub-1", None, None, None)
            .unwrap();
        let result = db.create_external_identity(user.id, provider.id, "sub-1", None, None, None);
        assert!(
            result.is_err(),
            "Duplicate (provider_id, external_sub) should fail"
        );
    }

    #[test]
    fn test_update_provider_preserves_unmodified_fields() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let mut req = make_provider_req("prov");
        req.display_name = Some("Original Name".into());
        req.priority = 42;
        let created = db.create_auth_provider(&req).unwrap();

        // Update only enabled
        let updated = db
            .update_auth_provider(
                created.id,
                &UpdateAuthProviderRequest {
                    name: None,
                    provider_type: None,
                    enabled: Some(false),
                    priority: None,
                    display_name: None,
                    client_id: None,
                    client_secret: None,
                    issuer_url: None,
                    scopes: None,
                    extra_config: None,
                },
            )
            .unwrap();

        assert!(!updated.enabled);
        assert_eq!(updated.display_name.as_deref(), Some("Original Name"));
        assert_eq!(updated.priority, 42);
        assert_eq!(updated.client_id.as_deref(), Some("cid"));
    }

    #[test]
    fn test_load_providers_ordered_by_priority() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let mut r1 = make_provider_req("low");
        r1.priority = 1;
        let mut r2 = make_provider_req("high");
        r2.priority = 10;
        let mut r3 = make_provider_req("mid");
        r3.priority = 5;

        db.create_auth_provider(&r1).unwrap();
        db.create_auth_provider(&r2).unwrap();
        db.create_auth_provider(&r3).unwrap();

        let providers = db.load_auth_providers().unwrap();
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].name, "high"); // priority 10 first (DESC)
        assert_eq!(providers[1].name, "mid"); // priority 5
        assert_eq!(providers[2].name, "low"); // priority 1
    }

    #[test]
    fn test_auth_source_default_local_for_normal_users() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let user = db.create_user("normal", "AK1", "SK1", true, &[]).unwrap();
        assert_eq!(user.auth_source, "local");
    }

    #[test]
    fn test_set_group_memberships_empty_clears_all() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let user = db.create_user("u", "AK1", "SK1", true, &[]).unwrap();
        let g1 = db.create_group("g1", "", &[]).unwrap();
        let g2 = db.create_group("g2", "", &[]).unwrap();

        db.set_user_group_memberships(user.id, &[g1.id, g2.id])
            .unwrap();
        assert_eq!(db.get_user_group_ids(user.id).unwrap().len(), 2);

        // Set to empty
        db.set_user_group_memberships(user.id, &[]).unwrap();
        assert!(db.get_user_group_ids(user.id).unwrap().is_empty());
    }

    #[test]
    fn test_set_user_group_memberships() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let user = db
            .create_user("carol", "AKCAROL1", "secret", true, &[])
            .unwrap();
        let g1 = db.create_group("group-a", "", &[]).unwrap();
        let g2 = db.create_group("group-b", "", &[]).unwrap();
        let g3 = db.create_group("group-c", "", &[]).unwrap();

        // Set initial memberships
        db.set_user_group_memberships(user.id, &[g1.id, g2.id])
            .unwrap();
        let ids = db.get_user_group_ids(user.id).unwrap();
        assert_eq!(ids.len(), 2);

        // Replace with different set
        db.set_user_group_memberships(user.id, &[g2.id, g3.id])
            .unwrap();
        let ids = db.get_user_group_ids(user.id).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&g2.id));
        assert!(ids.contains(&g3.id));
        assert!(!ids.contains(&g1.id));
    }
}
