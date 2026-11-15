use std::time::{Duration, SystemTime, UNIX_EPOCH};

use main_core::shell::file_index::decider::{Aborted, JevResponse, JevTransport, fetch_transport};

pub(super) struct DecisionTransport {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl DecisionTransport {
    pub(super) fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|error| error.to_string())?;
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()
            .map_err(|error| error.to_string())?;
        Ok(Self { runtime, client })
    }
}

impl JevTransport for DecisionTransport {
    fn now_ms(&mut self) -> f64 {
        SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before Unix epoch").as_millis() as f64
    }

    fn send(&mut self, url: &str, body: &str, key: &str, abort_at_ms: f64) -> Result<JevResponse, String> {
        let remaining = abort_at_ms - self.now_ms();
        if !remaining.is_finite() || remaining <= 0.0 { return Err("cancelled".into()); }
        let client = &self.client;
        let runtime = &self.runtime;
        fetch_transport(url, body, key, &mut |request| {
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs_f64(remaining / 1000.0), async {
                    let mut builder = client.post(&request.url).body(request.body.clone());
                    for (name, value) in &request.headers { builder = builder.header(name.as_str(), value.as_str()); }
                    let mut response = builder.send().await.map_err(|error| error.to_string())?;
                    let status = f64::from(response.status().as_u16());
                    let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()).map(str::to_owned);
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                        if bytes.len() + chunk.len() > 1024 * 1024 { return Err("response-too-large".into()); }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(JevResponse { status, retry_after, body: String::from_utf8_lossy(&bytes).into_owned() })
                }).await.map_err(|_| "cancelled".to_owned())?
            })
        })
    }

    fn sleep(&mut self, ms: f64, abort_at_ms: f64) -> Result<(), Aborted> {
        let remaining = abort_at_ms - self.now_ms();
        if !ms.is_finite() || ms < 0.0 || remaining <= ms { return Err(Aborted); }
        std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
        Ok(())
    }
}
