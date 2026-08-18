//! Untrusted JWT payload decode for identity claims only.

use serde_json::Value;

pub fn decode_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    if parts.next().is_none() || parts.next().is_some() {
        return None;
    }
    let bytes = decode_b64url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

pub fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = decode_payload(token)?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn decode_b64url(input: &str) -> Option<Vec<u8>> {
    let mut normalized = input.replace('-', "+").replace('_', "/");
    while !normalized.len().is_multiple_of(4) {
        normalized.push('=');
    }
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, normalized).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn jwt(payload: &str) -> String {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("aaa.{body}.sig")
    }

    #[test]
    fn extracts_chatgpt_account_id() {
        let token = jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_9"}}"#);
        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acct_9"));
        assert!(chatgpt_account_id("not-a-jwt").is_none());
    }
}
