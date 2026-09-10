//! HTTP 호출 (ureq, 타임아웃 필수).

use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
}

pub fn call(
    _method: &str,
    _url: &str,
    _headers: &BTreeMap<String, String>,
    _body: Option<&str>,
    _timeout: Duration,
) -> anyhow::Result<HttpResponse> {
    anyhow::bail!("HTTP 미구현")
}
