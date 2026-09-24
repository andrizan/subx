use anyhow::{Context, Result};
use futures::{StreamExt, stream};
use reqwest::Client;

use super::{LangPair, Translator, mask_key, prompt};

/// OpenAI-compatible chat client: one implementation serves OpenAI,
/// OpenRouter, DeepSeek, Gemini, local Ollama/LM Studio, opencode and any
/// other OpenAI-compatible service — only `--base-url` changes.
pub struct AiTranslator {
    client: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    batch_size: usize,
    context: usize,
    concurrency: usize,
    rate_limit_ms: u64,
    glossary: Vec<(String, String)>,
}

impl AiTranslator {
    pub fn new(client: Client, base_url: String, model: String, api_key: Option<String>) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            batch_size: 10,
            context: 2,
            concurrency: 2,
            rate_limit_ms: 0,
            glossary: Vec::new(),
        }
    }

    pub fn with_batch(mut self, n: usize, context: usize) -> Self {
        self.batch_size = n.max(1);
        self.context = context;
        self
    }

    pub fn with_glossary(mut self, glossary: Vec<(String, String)>) -> Self {
        self.glossary = glossary;
        self
    }

    /// Best-effort pacing between requests.
    pub fn with_rate_limit(mut self, ms: u64) -> Self {
        self.rate_limit_ms = ms;
        self
    }

    /// Resolve the key: explicit flag -> provider env -> SUBX_API_KEY.
    /// Local servers (localhost/127.0.0.1) need no key. Never logs the key.
    pub fn resolve_api_key(explicit: Option<String>, base_url: &str) -> Result<Option<String>> {
        if let Some(k) = explicit
            && !k.trim().is_empty()
        {
            return Ok(Some(k));
        }
        let lower = base_url.to_lowercase();
        let names: &[&str] = if lower.contains("openrouter") {
            &["OPENROUTER_API_KEY"]
        } else if lower.contains("deepseek") {
            &["DEEPSEEK_API_KEY"]
        } else if lower.contains("google") || lower.contains("gemini") {
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        } else if lower.contains("openai") {
            &["OPENAI_API_KEY"]
        } else {
            &[]
        };
        for n in names.iter().chain(["SUBX_API_KEY"].iter()) {
            if let Ok(v) = std::env::var(n)
                && !v.trim().is_empty()
            {
                return Ok(Some(v));
            }
        }
        if lower.contains("localhost") || lower.contains("127.0.0.1") || lower.contains("[::1]") {
            return Ok(None);
        }
        anyhow::bail!(
            "no API key: pass --api-key or set {} / SUBX_API_KEY",
            names.first().unwrap_or(&"SUBX_API_KEY")
        )
    }

    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let endpoint = format!("{}/chat/completions", self.base_url);
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(k) = &self.api_key {
            let v = format!("Bearer {k}");
            headers.insert(
                reqwest::header::AUTHORIZATION,
                v.parse().context("bad API key characters")?,
            );
        }
        let mut last: anyhow::Error = anyhow::anyhow!("no attempts made");
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    1000u64 * (1u64 << attempt),
                ))
                .await;
            }
            let resp = match self
                .client
                .post(&endpoint)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last = e.into();
                    continue;
                }
            };
            let status = resp.status();
            if status.as_u16() == 401 {
                anyhow::bail!(
                    "AI provider rejected the key (401) for model '{}' (key {})",
                    self.model,
                    self.api_key.as_deref().map(mask_key).unwrap_or_default()
                );
            }
            if status.as_u16() == 429 || status.is_server_error() {
                last = anyhow::anyhow!("AI provider HTTP {status}, retrying");
                continue;
            }
            if !status.is_success() {
                let snippet: String = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                anyhow::bail!("AI provider HTTP {status}: {snippet}");
            }
            let v: serde_json::Value = resp.json().await.context("bad AI JSON")?;
            let content = v
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .context("AI response lacks choices[0].message.content")?;
            return Ok(content.to_string());
        }
        Err(last)
    }

    async fn translate_chunk(
        &self,
        targets: &[String],
        before: &[String],
        after: &[String],
        pair: &LangPair,
    ) -> Result<Vec<String>> {
        let system = prompt::system_prompt(&pair.from, &pair.to, &self.glossary);
        let user = prompt::user_block(targets, before, after);
        let mut last: anyhow::Error = anyhow::anyhow!("no attempts made");
        for _ in 0..2 {
            let reply = self.chat(&system, &user).await?;
            match prompt::parse_numbered(&reply, targets.len()) {
                Ok(lines) => {
                    if self.rate_limit_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(self.rate_limit_ms))
                            .await;
                    }
                    return Ok(lines);
                }
                Err(e) => last = e,
            }
        }
        Err(last.context("AI reply did not match the requested line count"))
    }
}

impl Translator for AiTranslator {
    async fn translate_batch(&self, texts: &[String], pair: &LangPair) -> Result<Vec<String>> {
        // Chunk with ±context windows; only target lines are translated.
        let mut chunks: Vec<(Vec<String>, Vec<String>, Vec<String>)> = Vec::new();
        let mut start = 0;
        while start < texts.len() {
            let end = (start + self.batch_size).min(texts.len());
            let from = start.saturating_sub(self.context);
            let to = (end + self.context).min(texts.len());
            chunks.push((
                texts[start..end].to_vec(),
                texts[from..start].to_vec(),
                texts[end..to].to_vec(),
            ));
            start = end;
        }
        let done: Vec<(usize, Result<Vec<String>>)> = stream::iter(chunks.into_iter().enumerate())
            .map(|(i, (t, b, a))| async move { (i, self.translate_chunk(&t, &b, &a, pair).await) })
            .buffer_unordered(self.concurrency)
            .collect()
            .await;
        let mut ordered: Vec<Vec<String>> = vec![Vec::new(); done.len()];
        for (i, r) in done {
            ordered[i] = r?;
        }
        Ok(ordered.into_iter().flatten().collect())
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

    /// Mock chat server: translates each numbered user line to `N. TR:<rest>`.
    async fn echo_chat_server(n: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut data = Vec::new();
                let mut buf = [0u8; 8192];
                let body = loop {
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
                };
                let mut out = Vec::new();
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                    let msgs = v
                        .get("messages")
                        .and_then(|m| m.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let user = msgs
                        .iter()
                        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                        .filter_map(|m| {
                            m.get("content")
                                .and_then(|c| c.as_str())
                                .map(str::to_string)
                        })
                        .next()
                        .unwrap_or_default();
                    for line in user.lines() {
                        let t = line.trim();
                        if let Some((num, rest)) = t.split_once(['.', ')']) {
                            if !num.trim().is_empty()
                                && num.trim().bytes().all(|b| b.is_ascii_digit())
                            {
                                out.push(format!("{}. TR:{}", num.trim(), rest.trim()));
                            }
                        }
                    }
                }
                let content = out.join("\n");
                let reply_body =
                    serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply_body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(reply_body.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn resolves_keys() {
        // Explicit flag always wins (no env reads on this path).
        let k =
            AiTranslator::resolve_api_key(Some("sk-test".to_string()), "https://api.openai.com/v1")
                .unwrap();
        assert_eq!(k.as_deref(), Some("sk-test"));
        // Local servers need no key.
        let k = AiTranslator::resolve_api_key(None, "http://localhost:11434/v1").unwrap();
        assert_eq!(k, None);
    }

    /// 3 texts at batch 2 -> 2 chunks; placeholders survive the round trip.
    #[tokio::test]
    async fn translates_batches_with_context() {
        let server = echo_chat_server(2).await;
        let t =
            AiTranslator::new(Client::new(), server, "m".into(), Some("k".into())).with_batch(2, 1);
        let pair = LangPair {
            from: "en".into(),
            to: "id".into(),
        };
        let texts = vec!["Hello ⟦0⟧".to_string(), "World".into(), "Again".into()];
        let out = t.translate_batch(&texts, &pair).await.unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|l| l.starts_with("TR:")), "{out:?}");
        assert!(out[0].contains("⟦0⟧"));
    }
}
