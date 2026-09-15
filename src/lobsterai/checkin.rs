//! Automatic daily check-in (+100 credits per account per day) and credit
//! queries, using the real client-activities endpoints the desktop client
//! exercises:
//!
//! 1. `GET /api/client-activities/slot?placement=desktop_sidebar&…`
//!    → `{slotState: "available", activity: {activityCode, configRevision}}`
//!    (`slotState=empty` also means already-claimed/ineligible; a low
//!    client version yields empty — the UA is load-bearing);
//! 2. `GET /api/client-activities/{code}/context?configRevision={rev}`
//!    → `{state: {claimedToday}, actions: ["check_in", …]}`;
//! 3. `POST /api/client-activities/{code}/actions/check_in`
//!    → `{configRevision, idempotencyKey: <uuid4>, payload: {}}`
//!    → result carries granted credits.
//!
//! Credits come from `GET /api/user/profile-summary`
//! (`totalCreditsRemaining`, includes campaign credits).

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::credential::Credential;
use crate::error::ApiError;

const CHECKIN_TIMEOUT_SECS: u64 = 30;

/// One check-in attempt outcome.
#[derive(Debug, Clone)]
pub struct CheckinResult {
    pub ok: bool,
    /// True when the day was already claimed (not an error).
    pub already: bool,
    pub credits_granted: Option<f64>,
    pub message: String,
}

/// The upstream `{code, msg, data}` envelope.
fn parse_envelope(raw: &[u8]) -> Result<(i64, String, Value), String> {
    let payload: Value =
        serde_json::from_slice(raw).map_err(|e| format!("invalid JSON envelope: {e}"))?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let msg = payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok((code, msg, data))
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    credential: &Credential,
) -> Result<Value, String> {
    let response = http
        .get(url)
        .headers(super::headers::signed_headers(credential))
        .timeout(Duration::from_secs(CHECKIN_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let raw = response.bytes().await.map_err(|e| e.to_string())?;
    if status == 401 {
        return Err("unauthorized (token expired)".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let (code, msg, data) = parse_envelope(&raw)?;
    if code != 0 {
        return Err(format!("code={code} msg={msg}"));
    }
    if !data.is_object() {
        return Err("data is empty (token may have expired)".into());
    }
    Ok(data)
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    credential: &Credential,
    body: Value,
) -> Result<Value, String> {
    let response = http
        .post(url)
        .headers(super::headers::signed_headers(credential))
        .json(&body)
        .timeout(Duration::from_secs(CHECKIN_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let raw = response.bytes().await.map_err(|e| e.to_string())?;
    if status == 401 {
        return Err("unauthorized (token expired)".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let (code, msg, data) = parse_envelope(&raw)?;
    if code != 0 {
        return Err(format!("code={code} msg={msg}"));
    }
    Ok(data)
}

/// Slot query parameters — exactly the desktop client's shape.
const SLOT_QUERY: &str =
    "placement=desktop_sidebar&clientVersion=__VER__&containerApiVersion=2&platform=win32";

/// Run the full check-in flow for one account. Idempotent per day upstream
/// (`claimedToday` in the context) — safe to call on every schedule tick.
pub async fn daily_checkin(
    http: &reqwest::Client,
    base_url: &str,
    credential: &Credential,
) -> CheckinResult {
    // Step 1: activity slot.
    let query = SLOT_QUERY.replace("__VER__", super::credential::CLIENT_VERSION);
    let slot_url = format!("{base_url}/api/client-activities/slot?{query}");
    let slot = match get_json(http, &slot_url, credential).await {
        Ok(slot) => slot,
        Err(err) => {
            return CheckinResult {
                ok: false,
                already: false,
                credits_granted: None,
                message: err,
            }
        }
    };
    if slot.get("slotState").and_then(Value::as_str) != Some("available") {
        return CheckinResult {
            ok: false,
            already: false,
            credits_granted: None,
            message: "no available activity (slotState != available)".into(),
        };
    }
    let Some(activity) = slot.get("activity").and_then(Value::as_object) else {
        return CheckinResult {
            ok: false,
            already: false,
            credits_granted: None,
            message: "no activity in slot".into(),
        };
    };
    let code = activity
        .get("activityCode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let revision = activity
        .get("configRevision")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code.is_empty() {
        return CheckinResult {
            ok: false,
            already: false,
            credits_granted: None,
            message: "activity code missing".into(),
        };
    }

    // Step 2: activity context (claimedToday / available actions).
    let context_url =
        format!("{base_url}/api/client-activities/{code}/context?configRevision={revision}");
    let context = match get_json(http, &context_url, credential).await {
        Ok(context) => context,
        Err(err) => {
            return CheckinResult {
                ok: false,
                already: false,
                credits_granted: None,
                message: err,
            }
        }
    };
    let claimed_today = context
        .pointer("/state/claimedToday")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_checkin = context
        .get("actions")
        .and_then(Value::as_array)
        .is_some_and(|actions| actions.iter().any(|a| a.as_str() == Some("check_in")));
    if claimed_today || !has_checkin {
        return CheckinResult {
            ok: false,
            already: claimed_today,
            credits_granted: None,
            message: if claimed_today {
                "already claimed today".into()
            } else {
                "check_in action not available".into()
            },
        };
    }

    // Step 3: check-in action with an idempotency key.
    let action_url = format!("{base_url}/api/client-activities/{code}/actions/check_in");
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    let body = json!({
        "configRevision": revision,
        "idempotencyKey": idempotency_key,
        "payload": {},
    });
    let result = match post_json(http, &action_url, credential, body).await {
        Ok(result) => result,
        Err(err) => {
            return CheckinResult {
                ok: false,
                already: false,
                credits_granted: None,
                message: err,
            }
        }
    };
    let result_obj = result.get("result").cloned().unwrap_or(result.clone());
    let credits_granted = ["creditsGranted", "rewardCredits", "credits"]
        .iter()
        .find_map(|key| {
            result_obj
                .get(*key)
                .and_then(Value::as_f64)
                .filter(|v| *v > 0.0)
        });
    CheckinResult {
        ok: true,
        already: false,
        credits_granted,
        message: "check-in ok".into(),
    }
}

/// Remaining credits from `GET /api/user/profile-summary`.
pub async fn fetch_credits(
    http: &reqwest::Client,
    base_url: &str,
    credential: &Credential,
) -> Result<f64, ApiError> {
    let url = format!("{base_url}/api/user/profile-summary");
    let summary = get_json(http, &url, credential)
        .await
        .map_err(|e| ApiError::upstream(502, crate::redaction::sanitize_text(&e, &[])))?;
    let credits = summary
        .get("totalCreditsRemaining")
        .and_then(Value::as_f64)
        .filter(|v| *v >= 0.0)
        .ok_or_else(|| ApiError::upstream(502, "profile-summary carried no credits"))?;
    Ok(credits)
}

/// Check in every healthy account, record results, and refresh learned
/// credits in the pool. Called by the housekeeping loop.
pub async fn checkin_all(pool: &Arc<super::pool::Pool>, http: &reqwest::Client, base_url: &str) {
    let credentials: Vec<Arc<Credential>> = {
        let snapshot = pool.snapshot("");
        snapshot
            .iter()
            .filter_map(|entry| entry.get("name").and_then(Value::as_str))
            .filter_map(|name| pool.credential_by_safe_name(name))
            .collect()
    };
    for credential in credentials {
        let result = daily_checkin(http, base_url, &credential).await;
        tracing::info!(
            credential = %credential.safe_name,
            ok = result.ok,
            already = result.already,
            credits = result.credits_granted.unwrap_or(0.0),
            message = %result.message,
            "daily check-in"
        );
        if result.message.starts_with("unauthorized") {
            // Stale token: refresh once and retry the check-in.
            if super::refresh::force_refresh(&credential, http, base_url)
                .await
                .is_ok()
            {
                let retry = daily_checkin(http, base_url, &credential).await;
                tracing::info!(
                    credential = %credential.safe_name,
                    ok = retry.ok,
                    already = retry.already,
                    credits = retry.credits_granted.unwrap_or(0.0),
                    "daily check-in retry after refresh"
                );
            }
        }
        // Refresh learned credits (drives highest-credits-first selection).
        if let Ok(credits) = fetch_credits(http, base_url, &credential).await {
            pool.set_credits(&credential.uid(), credits as i64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_query_matches_desktop_client_shape() {
        let query = SLOT_QUERY.replace("__VER__", "2026.9.4");
        assert!(query.contains("placement=desktop_sidebar"));
        assert!(query.contains("clientVersion=2026.9.4"));
        assert!(query.contains("containerApiVersion=2"));
        assert!(query.contains("platform=win32"));
    }

    #[test]
    fn envelope_parsing() {
        let (code, msg, data) = parse_envelope(br#"{"code":0,"msg":"","data":{"a":1}}"#).unwrap();
        assert_eq!(code, 0);
        assert_eq!(msg, "");
        assert_eq!(data["a"], 1);
        let (code, msg, _) =
            parse_envelope(br#"{"code":40100,"message":"token invalid"}"#).unwrap();
        assert_eq!(code, 40100);
        assert_eq!(msg, "token invalid");
        assert!(parse_envelope(b"not json").is_err());
    }
}
