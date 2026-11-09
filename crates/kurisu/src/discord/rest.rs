use crate::channels::{Address, Call, Channel};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};

pub use crate::channels::Uncertain;

pub struct Rest {
    pub api: String,
    token: String,
    http: reqwest::Client,
    gates: Mutex<HashMap<String, Arc<Mutex<Instant>>>>,
    global: Mutex<Instant>,
    pub interval: Duration,
}

impl Rest {
    pub fn new(token: String, api: String) -> Result<Self> {
        if token.is_empty() {
            bail!("Discord token is not configured");
        }
        Ok(Self {
            api,
            token,
            http: reqwest::Client::builder()
                .user_agent("Kurisu Discord channel")
                .build()?,
            gates: Mutex::default(),
            global: Mutex::new(Instant::now()),
            interval: Duration::from_millis(1100),
        })
    }
    pub fn safe(&self, text: &str) -> String {
        text.replace(&self.token, "[REDACTED]")
    }
    pub async fn request(&self, method: reqwest::Method, path: &str, body: Value) -> Result<Value> {
        self.raw(method, path, Some(body), None).await
    }
    async fn raw(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        multipart: Option<(String, Vec<u8>)>,
    ) -> Result<Value> {
        let bucket = path
            .strip_prefix("/channels/")
            .and_then(|x| x.split('/').next())
            .map(str::to_string);
        let gate = if let Some(b) = bucket {
            Some(
                self.gates
                    .lock()
                    .await
                    .entry(b)
                    .or_insert_with(|| Arc::new(Mutex::new(Instant::now())))
                    .clone(),
            )
        } else {
            None
        };
        let mut guard = match &gate {
            Some(g) => Some(g.lock().await),
            None => None,
        };
        for _ in 0..6 {
            let global = if path.starts_with("/interactions/") {
                Instant::now()
            } else {
                *self.global.lock().await
            };
            let ready = guard.as_ref().map_or(global, |g| (**g).max(global));
            tokio::time::sleep_until(ready).await;
            let mut request = self
                .http
                .request(method.clone(), format!("{}{}", self.api, path))
                .header("Authorization", format!("Bot {}", self.token))
                .timeout(Duration::from_secs(60));
            if let Some((ctype, bytes)) = &multipart {
                request = request.header("Content-Type", ctype).body(bytes.clone());
            } else if let Some(b) = &body {
                request = request.json(b);
            }
            let response = request.send().await.map_err(|e| {
                if e.is_connect() {
                    anyhow!("Discord connection failed: {}", e.without_url())
                } else {
                    anyhow!(Uncertain)
                }
            })?;
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(g) = guard.as_mut() {
                **g = Instant::now() + self.interval;
            }
            let bytes = response.bytes().await.map_err(|_| anyhow!(Uncertain))?;
            let value: Value = if bytes.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            };
            let number = |key: &str| {
                headers
                    .get(key)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|n| n.is_finite() && *n >= 0.0)
            };
            if headers
                .get("x-ratelimit-remaining")
                .and_then(|s| s.to_str().ok())
                == Some("0")
            {
                if let (Some(g), Some(delay)) = (guard.as_mut(), number("x-ratelimit-reset-after"))
                {
                    **g = (Instant::now() + Duration::from_secs_f64(delay.min(3600.0))).max(**g);
                }
            }
            if status.as_u16() == 429 {
                let delay = value["retry_after"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .or_else(|| number("retry-after"))
                    .unwrap_or(1.0)
                    .min(3600.0);
                let until = Instant::now() + Duration::from_secs_f64(delay);
                if value["global"] == true {
                    *self.global.lock().await = until;
                }
                if let Some(g) = guard.as_mut() {
                    **g = until.max(**g);
                }
                tokio::time::sleep_until(until).await;
                continue;
            }
            if status.is_server_error() {
                return Err(anyhow!(Uncertain));
            }
            if !status.is_success() {
                bail!(
                    "Discord HTTP {} code {}: {}",
                    status.as_u16(),
                    value["code"].as_i64().unwrap_or(0),
                    self.safe(value["message"].as_str().unwrap_or("request rejected"))
                );
            }
            if value.is_null() {
                return Err(anyhow!(Uncertain));
            }
            return Ok(value);
        }
        bail!("Discord rate-limit retry budget exhausted")
    }
    pub async fn components(&self, to: &Address, id: &str, components: Value) -> Result<()> {
        self.request(
            reqwest::Method::PATCH,
            &format!("/channels/{}/messages/{id}", to.destination),
            json!({"components":components,"allowed_mentions":{"parse":[]}}),
        )
        .await?;
        Ok(())
    }
    pub async fn acknowledge(
        &self,
        id: &str,
        token: &str,
        kind: u64,
        data: Option<Value>,
    ) -> Result<()> {
        // Never log this path: it contains a short-lived interaction credential.
        let mut body = json!({"type":kind});
        if let Some(d) = data {
            body["data"] = d;
        }
        self.request(
            reqwest::Method::POST,
            &format!("/interactions/{id}/{token}/callback"),
            body,
        )
        .await
        .map_err(|_| anyhow!("Discord interaction acknowledgement failed"))?;
        Ok(())
    }
}

impl Channel for Rest {
    fn name(&self) -> &'static str {
        "discord"
    }
    fn redact(&self, text: &str) -> String {
        self.safe(text)
    }
    fn choices<'a>(
        &'a self,
        to: &'a Address,
        message: &'a str,
        options: Vec<(String, String)>,
    ) -> Call<'a, ()> {
        Box::pin(async move {
            let options:Vec<Value>=options.into_iter().take(25).map(|(label,value)|json!({"label":label.chars().take(90).collect::<String>(),"value":value})).collect();
            self.components(to,message,json!([{"type":1,"components":[{"type":3,"custom_id":"model-choice","placeholder":"Choose an option","options":options}]}])).await
        })
    }
    fn send<'a>(&'a self, to: &'a Address, text: &'a str, nonce: &'a str) -> Call<'a, String> {
        Box::pin(async move {
            let text = self.safe(text);
            if text.encode_utf16().count() > 2000 {
                bail!("Discord text exceeds 2000 characters");
            }
            let v=self.request(reqwest::Method::POST,&format!("/channels/{}/messages",to.destination),json!({"content":text,"nonce":nonce,"enforce_nonce":true,"allowed_mentions":{"parse":[]},"flags":4})).await?;
            v["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!(Uncertain))
        })
    }
    fn edit<'a>(&'a self, to: &'a Address, id: &'a str, text: &'a str) -> Call<'a, ()> {
        Box::pin(async move {
            let text = self.safe(text);
            if text.encode_utf16().count() > 2000 {
                bail!("Discord text exceeds 2000 characters");
            }
            self.request(
                reqwest::Method::PATCH,
                &format!("/channels/{}/messages/{id}", to.destination),
                json!({"content":text,"allowed_mentions":{"parse":[]},"flags":4}),
            )
            .await?;
            Ok(())
        })
    }
    fn file<'a>(
        &'a self,
        to: &'a Address,
        path: &'a Path,
        caption: &'a str,
        nonce: &'a str,
    ) -> Call<'a, Value> {
        Box::pin(async move {
            let meta = std::fs::metadata(path)?;
            if !meta.is_file() || meta.len() > 10 * 1024 * 1024 {
                bail!("Discord attachments in this DM are limited to 10 MiB; use a smaller file or an authorized download link");
            }
            let bytes = tokio::fs::read(path).await?;
            if bytes
                .windows(self.token.len())
                .any(|part| part == self.token.as_bytes())
            {
                bail!("Refused to upload a file containing the Discord credential");
            }
            let filename = self.safe(path.file_name().and_then(|n| n.to_str()).unwrap_or("file"));
            let caption = self.safe(caption);
            let boundary = format!("kurisu-{}", uuid::Uuid::new_v4().simple());
            let fields=vec![("payload_json".into(),json!({"content":caption.chars().take(1000).collect::<String>(),"nonce":nonce,"enforce_nonce":true,"allowed_mentions":{"parse":[]},"attachments":[{"id":0,"filename":filename}]}).to_string())];
            let body = crate::telegram::tg::multipart_body(
                &boundary,
                &fields,
                "files[0]",
                &filename,
                "application/octet-stream",
                &bytes,
            );
            let v = self
                .raw(
                    reqwest::Method::POST,
                    &format!("/channels/{}/messages", to.destination),
                    None,
                    Some((format!("multipart/form-data; boundary={boundary}"), body)),
                )
                .await?;
            let mid = v["id"].as_str().ok_or_else(|| anyhow!(Uncertain))?;
            Ok(
                json!({"ok":true,"message_id":mid,"bytes":bytes.len(),"kind":if ["jpg","jpeg","png"].contains(&path.extension().and_then(|s|s.to_str()).unwrap_or("").to_ascii_lowercase().as_str()){"photo"}else{"document"}}),
            )
        })
    }
    fn download<'a>(&'a self, url: &'a str, max: usize) -> Call<'a, Vec<u8>> {
        Box::pin(async move {
            let parsed = reqwest::Url::parse(url)?;
            let local =
                self.api.starts_with("http://127.0.0.1:") && parsed.host_str() == Some("127.0.0.1");
            if !local
                && (parsed.scheme() != "https"
                    || !matches!(
                        parsed.host_str(),
                        Some("cdn.discordapp.com" | "media.discordapp.net")
                    ))
            {
                bail!("Refused attachment URL outside Discord CDN");
            }
            // Do not forward the bot Authorization header to CDN or redirects.
            let mut r = self
                .http
                .get(parsed)
                .timeout(Duration::from_secs(120))
                .send()
                .await
                .map_err(|e| anyhow!("Discord download: {}", e.without_url()))?;
            if !r.status().is_success() {
                bail!("Discord download HTTP {}", r.status().as_u16());
            }
            if r.content_length().is_some_and(|n| n > max as u64) {
                bail!("Discord attachment exceeds download limit");
            }
            let mut out = Vec::new();
            while let Some(bytes) = r
                .chunk()
                .await
                .map_err(|e| anyhow!("Discord download: {}", e.without_url()))?
            {
                if out.len().saturating_add(bytes.len()) > max {
                    bail!("Discord attachment exceeds download limit");
                }
                out.extend_from_slice(&bytes);
            }
            Ok(out)
        })
    }
}
