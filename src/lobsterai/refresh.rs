//! Single-flight credential refresh.
//!
//! Concurrent requests that discover an expiring/expired access token share
//! ONE refresh call: the first to grab the refresh mutex re-checks expiry and
//! performs the refresh; everyone else waits and then observes the updated
//! token (double-check under lock). The result is atomically persisted.

use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::credential::{jwt_expiry, now_secs, Credential};
use crate::error::ApiError;

/// Refresh endpoint path on the LobsterAI backend.
pub const REFRESH_PATH: &str = "/api/auth/refresh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Token was refreshed by this call.
    Refreshed,
    /// Another caller refreshed first; the token is now valid.
    AlreadyFresh,
}

/// Ensure the credential has a non-expiring access token; refresh at most once
/// per concurrent wave. Returns an [`ApiError`] on failure (already sanitized).
pub async fn ensure_fresh(
    credential: &Arc<Credential>,
    http: &Client,
    base_url: &str,
    margin_secs: i64,
) -> Result<RefreshOutcome, ApiError> {
    if !credential.needs_refresh(margin_secs, now_secs()) {
        return Ok(RefreshOutcome::AlreadyFresh);
    }
    // Single-flight: hold the gate while doing the network call.
    let _guard = credential.refresh_lock.lock().await;
    // Double-check under the lock.
    if !credential.needs_refresh(margin_secs, now_secs()) {
        return Ok(RefreshOutcome::AlreadyFresh);
    }
    perform_refresh(credential, http, base_url).await?;
    Ok(RefreshOutcome::Refreshed)
}

/// Force a refresh regardless of expiry (used after an upstream 401).
pub async fn force_refresh(
    credential: &Arc<Credential>,
    http: &Client,
    base_url: &str,
) -> Result<(), ApiError> {
    // Single-flight as well: after a 401 many requests may fail together.
    let _guard = credential.refresh_lock.lock().await;
    perform_refresh(credential, http, base_url).await
}

async fn perform_refresh(
    credential: &Arc<Credential>,
    http: &Client,
    base_url: &str,
) -> Result<(), ApiError> {
    let url = format!("{base_url}{REFRESH_PATH}");
    let refresh_token = credential.refresh_token();
    if refresh_token.is_empty() {
        return Err(ApiError::authentication(
            "credential has no refresh token; please log in again (lobsterai-proxy login)",
        ));
    }
    let (uuid, user_id, first_keyfrom) =
        credential.with_data(|d| (d.uuid.clone(), d.user_id.clone(), d.first_keyfrom.clone()));
    let mut body = json!({
        "refreshToken": refresh_token,
        "firstKeyfrom": first_keyfrom,
        "latestKeyfrom": now_millis_string(),
        "version": super::credential::CLIENT_VERSION,
    });
    if !uuid.is_empty() {
        body["uuid"] = json!(uuid);
    }
    if !user_id.is_empty() {
        body["userId"] = json!(user_id);
    }

    let response = http
        .post(&url)
        .headers(super::headers::auth_headers())
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| ApiError::upstream(502, sanitize_reqwest_error("token refresh failed", &e)))?;

    let status = response.status().as_u16();
    let raw = response.bytes().await.unwrap_or_default();
    let access_token = credential.access_token();
    let secrets = [access_token.as_str(), refresh_token.as_str()];
    if !(200..300).contains(&status) {
        return Err(ApiError::upstream(
            status,
            crate::redaction::sanitize_text(
                &format!(
                    "token refresh failed with HTTP {status}: {}",
                    String::from_utf8_lossy(&raw)
                ),
                &secrets,
            ),
        ));
    }
    // Envelope: {code, msg, data}; code != 0 is a business error.
    let payload: Value = serde_json::from_slice(&raw)
        .map_err(|_| ApiError::upstream(502, "token refresh returned invalid JSON"))?;
    let code = payload
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| {
            // Tolerate a bare data object without the envelope.
            if payload.get("data").is_some() {
                0
            } else {
                -1
            }
        });
    if code != 0 {
        let message = payload
            .get("msg")
            .or_else(|| payload.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown refresh error");
        return Err(ApiError::authentication(format!(
            "token refresh rejected (code={code}): {}",
            crate::redaction::sanitize_text(message, &secrets)
        )));
    }
    let data = payload.get("data").cloned().unwrap_or(payload.clone());
    let new_access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if new_access.is_empty() {
        return Err(ApiError::authentication(
            "refresh response carried no accessToken — re-login required (lobsterai-proxy login)",
        ));
    }
    let new_refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or(refresh_token);
    let expires_at = data
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .map(|secs| now_secs() + secs)
        .or_else(|| jwt_expiry(&new_access))
        .unwrap_or_else(|| now_secs() + 30 * 86400);
    credential
        .apply_refresh(&new_access, &new_refresh, expires_at)
        .map_err(|e| ApiError::upstream(500, format!("credential persistence failed: {e}")))?;
    tracing::info!(credential = %credential.safe_name, "access token refreshed");
    Ok(())
}

fn now_millis_string() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format!("{millis}")
}

fn sanitize_reqwest_error(context: &str, error: &reqwest::Error) -> String {
    // reqwest errors can embed URLs; never include tokens (URLs here do not).
    format!("{context}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_path_is_stable() {
        assert_eq!(REFRESH_PATH, "/api/auth/refresh");
    }
}
