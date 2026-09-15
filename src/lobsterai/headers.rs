//! LobsterAI backend request headers. Clients can never influence these —
//! they are built from scratch per request and no client headers are
//! forwarded upstream.

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
};

use super::credential::{Credential, CLIENT_CAPABILITIES, USER_AGENT as LB_USER_AGENT};

/// Static client headers shared by every API call.
pub fn static_headers() -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    map.insert(USER_AGENT, HeaderValue::from_static(LB_USER_AGENT));
    map
}

/// Chat request headers: auth + client capability advertisement.
pub fn chat_headers(credential: &Credential) -> HeaderMap {
    let mut map = static_headers();
    let token = credential.access_token();
    let mut bearer = HeaderValue::from_bytes(format!("Bearer {token}").as_bytes())
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer "));
    bearer.set_sensitive(true);
    map.insert(AUTHORIZATION, bearer);
    insert(
        &mut map,
        "X-LobsterAI-Client-Capabilities",
        CLIENT_CAPABILITIES,
    );
    insert(
        &mut map,
        "X-LobsterAI-Client-Version",
        super::credential::CLIENT_VERSION,
    );
    map
}

/// Auth endpoints (exchange/refresh) carry no Bearer token.
pub fn auth_headers() -> HeaderMap {
    static_headers()
}

/// Signed-API headers (check-in/credits/models): auth + desktop client UA.
pub fn signed_headers(credential: &Credential) -> HeaderMap {
    let mut map = static_headers();
    let token = credential.access_token();
    let mut bearer = HeaderValue::from_bytes(format!("Bearer {token}").as_bytes())
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer "));
    bearer.set_sensitive(true);
    map.insert(AUTHORIZATION, bearer);
    map.insert(
        USER_AGENT,
        HeaderValue::from_static(super::credential::USER_AGENT),
    );
    map
}

fn insert(map: &mut HeaderMap, name: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_bytes(value.as_bytes()),
    ) {
        map.insert(name, value);
    }
}

/// Header names that must never be influenced by client input. The proxy
/// constructs upstream headers itself, so nothing from the client request
/// (including these) is ever forwarded.
pub const CLIENT_UNTRUSTED_HEADER_NAMES: [&str; 5] = [
    "authorization",
    "host",
    "content-length",
    "transfer-encoding",
    "cookie",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobsterai::credential::{parse_credential, Credential};
    use serde_json::json;

    fn test_credential() -> std::sync::Arc<Credential> {
        let dir = std::env::temp_dir().join(format!("lap-headers-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let data = parse_credential(json!({
            "auth": {"accessToken": "tok", "refreshToken": "rt", "expiresAt": 1},
            "account": {"uid": "u-1"}
        }))
        .unwrap();
        let path = dir.join("lobsterai-u-1.json");
        std::fs::write(&path, b"{}").unwrap();
        Credential::new(path, data, b"s")
    }

    #[test]
    fn chat_headers_carry_auth_and_capabilities() {
        let credential = test_credential();
        let map = chat_headers(&credential);
        assert_eq!(
            map.get("X-LobsterAI-Client-Capabilities").unwrap(),
            "kimi-k3-agentic-v1"
        );
        assert_eq!(map.get("user-agent").unwrap(), "LobsterAI/2026.9.4");
        let auth = map.get("authorization").unwrap();
        assert!(auth.to_str().unwrap().ends_with("tok"));
    }

    #[test]
    fn auth_headers_have_no_bearer() {
        let map = auth_headers();
        assert!(map.get("authorization").is_none());
        assert_eq!(map.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn untrusted_header_names_are_documented() {
        assert!(CLIENT_UNTRUSTED_HEADER_NAMES.contains(&"authorization"));
    }
}
