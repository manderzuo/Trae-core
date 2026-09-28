use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub key_id: String,
    pub scopes: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("API key is invalid or revoked")]
    InvalidApiKey,
    #[error("missing required scope: {scope}")]
    MissingScope { scope: String },
    #[error("core storage error: {0}")]
    Storage(#[from] crate::CoreError),
}

pub fn require_scope(principal: &Principal, scope: &str) -> Result<(), AuthError> {
    let video_read_is_implied_by_submit = scope == "videos:read" && principal.scopes.contains("videos:submit");
    if principal.scopes.contains(scope) || principal.scopes.contains("admin:*") || video_read_is_implied_by_submit {
        Ok(())
    } else {
        Err(AuthError::MissingScope {
            scope: scope.to_owned(),
        })
    }
}

impl crate::CoreStore {
    /// Current authorization for a request-scoped download capability. Does not
    /// depend on temporary encrypted input retention and never reveals a Key.
    pub fn active_principal_for_request(&self, request: &str) -> Result<Option<Principal>, crate::CoreError> {
        use rusqlite::OptionalExtension;
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let row = connection.query_row(
            "SELECT r.user_id,r.api_key_id,k.scopes_json FROM requests r
             JOIN api_keys k ON k.id=r.api_key_id AND k.user_id=r.user_id
             JOIN users u ON u.id=r.user_id
             WHERE r.id=?1 AND k.status='active' AND u.status='active'",
            [request], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)),
        ).optional()?;
        row.map(|(user_id,key_id,scopes)| Ok(Principal {user_id,key_id,scopes:serde_json::from_str(&scopes)?})).transpose()
    }
}
