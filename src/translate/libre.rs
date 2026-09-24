use anyhow::{Context, Result};
use futures::{StreamExt, stream};
use reqwest::Client;

use super::{LangPair, Translator};

/// LibreTranslate client (`POST {url}/translate`). Free and self-hostable:
/// `docker run -d -p 5000:5000 libretranslate/libretranslate`.
pub struct LibreTranslator {
    client: Client,
    url: String,
    api_key: Option<String>,
    concurrency: usize,
    rate_limit_ms: u64,
}

impl LibreTranslator {
    pub fn new(client: Client, url: String, api_key: Option<String>) -> Self {
        Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            api_key,
            concurrency: 4,
            rate_limit_ms: 0,
        }
    }

    /// Best-effort pacing between requests.
    pub fn with_rate_limit(mut self, ms: u64) -> Self {
        self.rate_limit_ms = ms;
        self
    }

    async fn translate_one(&self, text: &str, from: &str, to: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let mut body = serde_json::json!({
            "q": text, "source": from, "target": to, "format": "text",
        });
        if let Some(k) = &self.api_key {
            body["api_key"] = serde_json::Value::String(k.clone());
        }
        let endpoint = format!("{}/translate", self.url);
        let mut last: anyhow::Error = anyhow::anyhow!("no attempts made");
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500u64 * (1u64 << attempt)))
                    .await;
            }
            let resp = match self.client.post(&endpoint).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = e.into();
                    continue;
                }
            };
            let status = resp.status();
            if status.as_u16() == 429 || status.is_server_error() {
                last = anyhow::anyhow!("LibreTranslate HTTP {status}");
                continue;
            }
            if !status.is_success() {
                anyhow::bail!("LibreTranslate HTTP {status} (check URL, languages, API key)");
            }
            let v: serde_json::Value = resp.json().await.context("bad LibreTranslate JSON")?;
            let out = v
                .get("translatedText")
                .and_then(|t| t.as_str())
                .context("LibreTranslate response lacks translatedText")?;
            if self.rate_limit_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.rate_limit_ms)).await;
            }
            return Ok(out.to_string());
        }
        Err(last)
    }
}

impl Translator for LibreTranslator {
    async fn translate_batch(&self, texts: &[String], pair: &LangPair) -> Result<Vec<String>> {
        let done: Vec<(usize, Result<String>)> = stream::iter(texts.iter().enumerate())
            .map(|(i, t)| async move { (i, self.translate_one(t, &pair.from, &pair.to).await) })
            .buffer_unordered(self.concurrency)
            .collect()
            .await;
        let mut ordered = vec![String::new(); texts.len()];
        for (i, r) in done {
            ordered[i] = r?;
        }
        Ok(ordered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn headers_end(data: &[u8]) -> Option<usize> {
        data.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .filter_map(|l| {
                let (name, value) = l.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    value.trim().parse().ok()
                } else {
                    None
                }
            })
            .next()
            .unwrap_or(0)
    }

    async fn read_body(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let Ok(read) = sock.read(&mut buf).await else {
                break vec![];
            };
            if read == 0 {
                break vec![];
            }
            data.extend_from_slice(&buf[..read]);
            if let Some(hend) = headers_end(&data) {
                let len = content_length(&String::from_utf8_lossy(&data[..hend]));
                if data.len() >= hend + len {
                    break data[hend..hend + len].to_vec();
                }
            }
            if data.len() > 1_000_000 {
                break vec![];
            }
        }
    }

    async fn reply(sock: &mut tokio::net::TcpStream, body: &str) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
    }

    /// Mock LibreTranslate: echoes the posted `q` as `TR:<q>`, `n` requests.
    async fn echo_q_server(n: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let body = read_body(&mut sock).await;
                let q = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("q").and_then(|q| q.as_str()).map(str::to_string))
                    .unwrap_or_default();
                let out = serde_json::json!({"translatedText": format!("TR:{q}")}).to_string();
                reply(&mut sock, &out).await;
            }
        });
        format!("http://{addr}")
    }

    /// Order holds under concurrency because results map back by index.
    #[tokio::test]
    async fn translates_in_order() {
        let server = echo_q_server(2).await;
        let t = LibreTranslator::new(Client::new(), server, None);
        let pair = LangPair {
            from: "en".into(),
            to: "id".into(),
        };
        let out = t
            .translate_batch(&["Hello ⟦0⟧".into(), "World".into()], &pair)
            .await
            .unwrap();
        assert_eq!(out, ["TR:Hello ⟦0⟧", "TR:World"]);
    }
}
