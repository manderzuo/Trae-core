use std::sync::Arc;

use aiwork_core::{CoreStore, Principal};
use axum::{extract::{Request, State}, http::StatusCode, middleware::Next, response::{IntoResponse, Response}, Json};
use serde_json::json;

use crate::state::StarlinkRouterState;

pub fn require_core_principal(store: &CoreStore, authorization: Option<&str>) -> Result<Principal, Response> {
    let Some(value) = authorization else {
        return Err(auth_response(StatusCode::UNAUTHORIZED, "core_api_key_required", "请提供 Core API Key"));
    };
    let key = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")).unwrap_or(value).trim();
    if key.is_empty() {
        return Err(auth_response(StatusCode::UNAUTHORIZED, "core_api_key_required", "请提供 Core API Key"));
    }
    store.authenticate_api_key(key).map_err(|_| auth_response(StatusCode::UNAUTHORIZED, "invalid_api_key", "Core API Key 无效或已撤销"))
}

pub async fn user_auth(
    State(state): State<Arc<StarlinkRouterState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorization = request.headers().get("authorization").and_then(|value| value.to_str().ok());
    match require_core_principal(&state.store, authorization) {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(response) => response,
    }
}

pub fn auth_response(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": {"type": "authentication_error", "code": code, "message": message}}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::require_core_principal;
    use axum::http::StatusCode;

    #[test]
    fn missing_core_key_is_unauthorized() {
        let dir = std::env::temp_dir().join(format!("starlink-auth-{}", rand::random::<u64>()));
        let store = aiwork_core::CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        assert_eq!(require_core_principal(&store, None).unwrap_err().status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(dir);
    }
}
