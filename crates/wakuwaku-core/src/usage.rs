//! Provider-neutral usage presentation types.
//!
//! Provider-specific CLI plan fetchers were removed with the subprocess
//! drivers. Formatting and the optional rate-table HTTP helper remain because
//! usage history is a Waku-owned, provider-neutral feature.

use std::time::Duration;

use anyhow::{Context as _, anyhow};

pub use wakuwaku_protocol::usage::{format_tokens, reset_label};

/// Fetch a public JSON resource over HTTP.
///
/// Used only by the optional LiteLLM rate table. It is not a provider driver
/// and never receives an API key from the provider registry.
pub fn http_get(url: &str, headers: &[String]) -> anyhow::Result<(u16, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("could not start the usage rate-table HTTP client")?;
    let mut request = client.get(url);
    for header in headers {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| anyhow!("usage rate-table header {header:?} must be `Name: value`"))?;
        request = request.header(name.trim(), value.trim());
    }
    let response = request
        .send()
        .context("usage rate-table HTTP request failed")?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .context("usage rate-table HTTP body is not UTF-8")?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(status: &str, body: &str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_owned();
        let body = body.to_owned();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        port
    }

    #[test]
    fn http_get_returns_status_and_body_without_a_cli() {
        let port = serve_once("200 OK", "{\"ok\":true}");
        let (status, body) = http_get(
            &format!("http://127.0.0.1:{port}/rates"),
            &["Accept: application/json".to_owned()],
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
    }

    #[test]
    fn http_get_keeps_non_success_status() {
        let port = serve_once("404 Not Found", "missing");
        let (status, body) = http_get(&format!("http://127.0.0.1:{port}/missing"), &[]).unwrap();
        assert_eq!(status, 404);
        assert_eq!(body, "missing");
    }

    #[test]
    fn http_get_rejects_a_header_without_a_colon() {
        let error = http_get("http://127.0.0.1:1/", &["Accept".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("Name: value"), "{error}");
    }
}
