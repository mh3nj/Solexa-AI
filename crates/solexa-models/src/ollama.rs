//! `OllamaProvider`, the first *real* `ModelProvider` (round 37).
//!
//! Every round before this one ran on `MockLocalProvider`, and every
//! README said so plainly rather than implying otherwise. The
//! orchestration underneath, planner, capability routing, context
//! scoping, verification, conflicts, sync, priority scheduling, was
//! always real; the inference was not. This closes that.
//!
//! It speaks Ollama's documented HTTP API against a daemon the user
//! already runs themselves (`http://127.0.0.1:11434` by default), which
//! is the same principle round 22 applied to git and round 30 applied to
//! GitHub: SOLEXA never ships or manages the external thing, it talks to
//! the one you already have. No API key, no account, no cloud, a local
//! daemon serving local weights.
//!
//! Three endpoints are used:
//! - `GET /api/tags`, what models are installed
//! - `POST /api/show`, per-model detail: real context length and
//!   capabilities (round 38; see `find_context_length`)
//! - `POST /api/generate`, completion, in two modes: `generate`'s
//!   one-shot `"stream": false`, and `generate_streaming`'s real
//!   `"stream": true` (round 39), which delivers each token to the
//!   caller as the daemon produces it instead of only at the end.
//!
//! **Honest testing note**: this sandbox has no Ollama daemon, so every
//! test here runs against a real `tokio` TCP server that replies with
//! recorded Ollama payloads. That genuinely exercises the request
//! construction, the wire framing, the JSON shapes, and every error
//! path, but it is *not* the same as having been run against real
//! Ollama, and the README says so.

use crate::http;
use crate::provider_util::stable_id_for;
use crate::{FinishReason, GenerationRequest, GenerationResult, ModelProvider};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use solexa_domain::{ModelMetadata, Modality, Uuid};
use std::collections::HashMap;
use std::sync::Mutex;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 11434;

pub struct OllamaProvider {
    host: String,
    port: u16,
    /// SOLEXA identifies models by `Uuid`; Ollama identifies them by
    /// name (`"llama3:8b"`). This holds the mapping discovered by
    /// `refresh`, so `generate` can translate back.
    models: Mutex<HashMap<Uuid, OllamaModel>>,
}

#[derive(Clone)]
struct OllamaModel {
    metadata: ModelMetadata,
    ollama_name: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    details: TagDetails,
}

#[derive(Deserialize, Default)]
struct TagDetails {
    #[serde(default)]
    parameter_size: String,
    #[serde(default)]
    quantization_level: String,
}

/// Round 38: the shape `POST /api/show` actually returns. `model_info`
/// is deliberately `HashMap<String, serde_json::Value>` rather than a
/// fixed struct, its keys are architecture-prefixed
/// (`llama.context_length`, `qwen2.context_length`, `gemma3.context_length`,
/// ...), so there is no single fixed field name to deserialize into.
/// `find_context_length` below is what actually reads it.
#[derive(Deserialize, Default)]
struct ShowResponse {
    #[serde(default)]
    model_info: HashMap<String, serde_json::Value>,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Any `model_info` key ending in `.context_length` names the model's
/// real context window, the prefix is the architecture family
/// (`llama`, `qwen2`, `gemma3`, ...) and varies per model, which is
/// exactly why this searches rather than looking up one fixed key.
fn find_context_length(model_info: &HashMap<String, serde_json::Value>) -> u32 {
    model_info
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| v.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    response: String,
    #[serde(default)]
    eval_count: Option<u32>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    /// Round 51: Ollama's name for the finish reason (`"stop"` or
    /// `"length"`).
    #[serde(default)]
    done_reason: Option<String>,
}

/// Round 50: adds Ollama's `options` object (`num_predict` is its name
/// for a max-token limit) to a `/api/generate` payload, only when the
/// request actually set something, so a request that sets neither
/// produces exactly the payload it always did.
fn with_ollama_options(mut payload: serde_json::Value, request: &GenerationRequest) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(n) = request.max_tokens {
        options.insert("num_predict".into(), serde_json::json!(n));
    }
    if let Some(t) = request.temperature {
        options.insert("temperature".into(), serde_json::json!(t));
    }
    if !options.is_empty() {
        payload["options"] = serde_json::Value::Object(options);
    }
    payload
}

/// One line of a `"stream": true` `/api/generate` response, Ollama
/// sends a sequence of these, newline-delimited, as it generates.
/// Every line but the last carries a `response` fragment; the last has
/// `done: true` and the same token counts `GenerateResponse` gets in
/// one shot. `error` is Ollama's own documented way of reporting a
/// mid-stream failure (e.g. the model unloaded) inside the stream
/// itself rather than as an HTTP error status, round 39 surfaces it
/// as a real `Err` rather than silently truncating output.
#[derive(Deserialize, Default)]
struct GenerateStreamLine {
    #[serde(default)]
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    eval_count: Option<u32>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    done_reason: Option<String>,
}

impl OllamaProvider {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port, models: Mutex::new(HashMap::new()) }
    }

    pub fn local() -> Self {
        Self::new(DEFAULT_HOST, DEFAULT_PORT)
    }

    /// `POST /api/show` for one model. Returns `Ok(default)` shape info
    /// (empty `model_info`, no capabilities) is signaled by the caller
    /// falling back on `.unwrap_or_default()`, not by this method
    /// swallowing the error, a genuine failure is still surfaced to
    /// whoever wants to see it (`refresh` chooses not to look).
    async fn show(&self, ollama_name: &str) -> Result<ShowResponse> {
        let payload = serde_json::to_vec(&serde_json::json!({ "model": ollama_name }))?;
        let body = http::request(&self.host, self.port, "/api/show", Some(&payload))
            .await
            .with_context(|| format!("getting model info for {ollama_name}"))?;
        serde_json::from_slice(&body).context("Ollama's /api/show response wasn't the expected JSON")
    }

    /// Asks the daemon what's installed and caches the result. Must be
    /// called before `list_models` returns anything, deliberately not
    /// done lazily inside `list_models`, because that method is sync
    /// (the `ModelProvider` trait defines it that way) and doing
    /// blocking network I/O from it would be exactly the kind of hidden
    /// cost the spec warns against.
    pub async fn refresh(&self) -> Result<usize> {
        let body = http::request(&self.host, self.port, "/api/tags", None)
            .await
            .with_context(|| format!("listing models from Ollama at {}:{}", self.host, self.port))?;
        let parsed: TagsResponse =
            serde_json::from_slice(&body).context("Ollama's /api/tags response wasn't the expected JSON")?;

        let mut models = HashMap::new();
        for entry in parsed.models {
            // A stable id derived from the model's name, so the same
            // installed model keeps the same SOLEXA id across restarts,
            // otherwise a Specialist's `preferred_model_ids` would go
            // stale every time the app reopened.
            let id = stable_id_for(&entry.name);

            // Round 38: one `/api/show` per model for the detail
            // `/api/tags` doesn't carry, real context length and real
            // capabilities, replacing round 37's honest "0 = unknown" /
            // "false = not guessed" placeholders with the daemon's own
            // answer. Deliberately non-fatal per model: one model's
            // `/api/show` failing (a transient daemon hiccup, an
            // unusual model that doesn't answer it) falls back to the
            // same honest placeholders rather than failing the whole
            // refresh over one entry.
            let show = self.show(&entry.name).await.unwrap_or_default();
            let context_capacity_tokens = find_context_length(&show.model_info);
            let tool_support = show.capabilities.iter().any(|c| c == "tools");
            let mut modalities = vec![Modality::Text];
            if show.capabilities.iter().any(|c| c == "vision") {
                modalities.push(Modality::Vision);
            }

            models.insert(
                id,
                OllamaModel {
                    metadata: ModelMetadata {
                        id,
                        name: entry.name.clone(),
                        // The domain crate anticipated real adapters
                        // with this field from Phase 1, this is the
                        // first value other than "mock" to ever go in.
                        provider: "ollama".into(),
                        modalities,
                        // Real, from `/api/show`'s `model_info`, see
                        // `find_context_length`. Still 0 (meaning
                        // *unknown*, not *zero-length*) on the rare
                        // model whose `/api/show` didn't return one.
                        context_capacity_tokens,
                        parameter_count_billions: parse_parameter_billions(&entry.details.parameter_size),
                        quantization: non_empty(entry.details.quantization_level.clone()),
                        // Ollama reports the on-disk size of the weights.
                        // Resident VRAM is in the same ballpark but not
                        // identical; the field is named an estimate and
                        // this is one.
                        estimated_vram_mb: (entry.size / 1_048_576).max(1) as u32,
                        estimated_ram_mb: (entry.size / 1_048_576).max(1) as u32,
                        // Real, from `/api/show`'s `capabilities` array
                        // no longer guessed. Ollama has no separate
                        // "structured output" capability distinct from
                        // plain completion (its JSON-schema formatting
                        // works via grammar-constrained sampling on
                        // essentially any model, not a per-model flag
                        // the API reports), so there is still no
                        // positive evidence to claim it from, left
                        // `false` rather than assumed `true` just
                        // because it would usually work.
                        tool_support,
                        structured_output_support: false,
                        // Ollama itself always supports streaming, even
                        // though this adapter doesn't use it yet.
                        streaming_support: true,
                        local: true,
                        license: None,
                    },
                    ollama_name: entry.name,
                },
            );
        }
        let count = models.len();
        *self.models.lock().unwrap() = models;
        Ok(count)
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `"8B"` / `"7.6B"` / `"350M"` → billions of parameters. Returns
/// `None` rather than guessing when Ollama reports something
/// unexpected.
fn parse_parameter_billions(raw: &str) -> Option<f32> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (number, unit) = raw.split_at(raw.len() - 1);
    let value: f32 = number.parse().ok()?;
    match unit.to_ascii_uppercase().as_str() {
        "B" => Some(value),
        "M" => Some(value / 1000.0),
        _ => None,
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn list_models(&self) -> Vec<ModelMetadata> {
        self.models.lock().unwrap().values().map(|m| m.metadata.clone()).collect()
    }

    async fn generate(&self, request: GenerationRequest) -> Result<GenerationResult> {
        let model = self
            .models
            .lock()
            .unwrap()
            .get(&request.model_id)
            .cloned()
            .ok_or_else(|| anyhow!("model {} isn't one this Ollama daemon reported, call refresh() first", request.model_id))?;

        let payload = serde_json::json!({
            "model": model.ollama_name,
            "prompt": request.prompt,
            "system": request.system_prompt,
            // One-shot: the whole reply comes back in a single
            // response body. See `generate_streaming` below for the
            // `"stream": true` counterpart (round 39) that delivers
            // partial output as the daemon produces it.
            "stream": false,
        });
        let payload = serde_json::to_vec(&with_ollama_options(payload, &request))?;

        let body = http::request(&self.host, self.port, "/api/generate", Some(&payload))
            .await
            .with_context(|| format!("generating with Ollama model {}", model.ollama_name))?;
        let parsed: GenerateResponse =
            serde_json::from_slice(&body).context("Ollama's /api/generate response wasn't the expected JSON")?;

        Ok(GenerationResult {
            // Real counts from the daemon when it reports them, rather
            // than the character-based estimate the mock provider uses.
            estimated_tokens: parsed
                .eval_count
                .unwrap_or_else(|| (parsed.response.len() / 4).max(1) as u32)
                + parsed.prompt_eval_count.unwrap_or(0),
            finish_reason: FinishReason::from_provider(parsed.done_reason.as_deref()),
            text: parsed.response,
        })
    }

    /// Real per-token streaming (round 39), `"stream": true` against
    /// the same `/api/generate` endpoint `generate` uses. Ollama
    /// responds with a sequence of newline-delimited JSON objects
    /// instead of one JSON body; each carries a `response` fragment
    /// until a final line with `done: true` and the same token counts
    /// `generate`'s one-shot response gets.
    ///
    /// `http::request_streaming`'s callback hands over raw decoded body
    /// bytes with no line alignment guaranteed, a single call might
    /// contain half a JSON line, one complete line, or several. Those
    /// are reassembled here into whole lines before parsing; that
    /// reassembly is this method's job, not `http`'s (see that module's
    /// own doc comment on the scope boundary).
    ///
    /// A malformed line, or a line carrying Ollama's own documented
    /// `error` field, sets `stream_error` and stops sending further
    /// chunks (the closure keeps consuming remaining bytes, it can't
    /// abort the underlying read loop, but stops acting on them);
    /// `stream_error` is checked once `request_streaming` returns and
    /// turned into the same kind of `Err` `generate` would produce for
    /// a comparable failure, rather than silently returning a truncated
    /// success.
    async fn generate_streaming(
        &self,
        request: GenerationRequest,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<GenerationResult> {
        let model = self
            .models
            .lock()
            .unwrap()
            .get(&request.model_id)
            .cloned()
            .ok_or_else(|| anyhow!("model {} isn't one this Ollama daemon reported, call refresh() first", request.model_id))?;

        let payload = serde_json::json!({
            "model": model.ollama_name,
            "prompt": request.prompt,
            "system": request.system_prompt,
            "stream": true,
        });
        let payload = serde_json::to_vec(&with_ollama_options(payload, &request))?;

        let mut full_text = String::new();
        let mut final_eval_count: Option<u32> = None;
        let mut final_prompt_eval_count: Option<u32> = None;
        let mut final_done_reason: Option<String> = None;
        let mut line_buf: Vec<u8> = Vec::new();
        let mut stream_error: Option<String> = None;

        http::request_streaming(&self.host, self.port, "/api/generate", Some(&payload), |data| {
            line_buf.extend_from_slice(data);
            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = line_buf.drain(..=pos).collect();
                // Drop the trailing '\n' (and a possible '\r' before it).
                let line = line[..line.len() - 1].to_vec();
                let line = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { &line[..] };
                if line.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                if stream_error.is_some() {
                    // Already failed on an earlier line this call,
                    // keep draining bytes (we can't stop the read loop
                    // from here) but stop acting on their content.
                    continue;
                }
                match serde_json::from_slice::<GenerateStreamLine>(line) {
                    Ok(parsed) => {
                        if let Some(err) = parsed.error {
                            stream_error = Some(err);
                            continue;
                        }
                        if !parsed.response.is_empty() {
                            full_text.push_str(&parsed.response);
                            let _ = chunks.send(parsed.response.clone());
                        }
                        if parsed.done {
                            final_eval_count = parsed.eval_count;
                            final_prompt_eval_count = parsed.prompt_eval_count;
                            final_done_reason = parsed.done_reason;
                        }
                    }
                    Err(e) => {
                        stream_error = Some(format!("unparseable line in Ollama's stream: {e}"));
                    }
                }
            }
        })
        .await
        .with_context(|| format!("generating with Ollama model {}", model.ollama_name))?;

        if let Some(err) = stream_error {
            bail!("Ollama's /api/generate stream failed: {err}");
        }

        Ok(GenerationResult {
            estimated_tokens: final_eval_count
                .unwrap_or_else(|| (full_text.len() / 4).max(1) as u32)
                + final_prompt_eval_count.unwrap_or(0),
            finish_reason: FinishReason::from_provider(final_done_reason.as_deref()),
            text: full_text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A real TCP server that returns one canned HTTP response and
    /// records the request it received. Not a mock object, an actual
    /// socket, so the client's framing and headers are genuinely
    /// exercised.
    async fn serve_once(response: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let received = String::from_utf8_lossy(&buf[..n]).to_string();
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.ok();
            received
        });
        (port, handle)
    }

    /// A real, multi-request fake Ollama: `/api/tags` gets `tags_body`
    /// every time; `POST /api/show` looks up its response by the
    /// `"model"` field in the request's own JSON body, the same way
    /// the real daemon distinguishes which model it's being asked
    /// about. `refresh()` needs this, it's a real N+1 request sequence
    /// (one `/api/tags`, one `/api/show` per model), which `serve_once`
    /// (exactly one connection, ever) can't stand in for.
    ///
    /// Returns the port plus a shared, live-updating request log and
    /// the task's `JoinHandle`. The log is a separate `Arc<Mutex<_>>`
    /// rather than the task's return value on purpose: the accept loop
    /// runs until the caller is done with it, which callers signal by
    /// calling `.abort()`, a task discards its return value when
    /// aborted, so anything worth reading afterward has to live outside
    /// it.
    fn serve_ollama(
        tags_body: &'static str,
        show_responses: HashMap<&'static str, &'static str>,
    ) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        // Bind synchronously via std so the returned port is valid
        // immediately, then hand the listener to a background task,
        // callers construct the provider with this port before the
        // task has necessarily started accepting.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        let listener = TcpListener::from_std(std_listener).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = requests.clone();
        let handle = tokio::spawn(async move {
            // One connection per request (the client sends
            // `Connection: close`), so each loop iteration is exactly
            // one HTTP request. Runs until the caller aborts the
            // handle, there's no natural request count to stop at,
            // since it depends on how many models the test's `/api/tags`
            // body lists.
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let Ok(n) = socket.read(&mut buf).await else { continue };
                if n == 0 {
                    continue;
                }
                let received = String::from_utf8_lossy(&buf[..n]).to_string();

                let response = if received.starts_with("GET /api/tags") {
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", tags_body.len(), tags_body)
                } else if received.starts_with("POST /api/show") {
                    let sent_body = received.split("\r\n\r\n").nth(1).unwrap_or("");
                    let requested: serde_json::Value = serde_json::from_str(sent_body).unwrap();
                    let model_name = requested["model"].as_str().unwrap_or("");
                    match show_responses.get(model_name) {
                        Some(body) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 22\r\n\r\n{\"error\":\"not found\"}".to_string(),
                    }
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                };

                requests_for_task.lock().unwrap().push(received);
                let _ = socket.write_all(response.as_bytes()).await;
                socket.flush().await.ok();
            }
        });
        (port, requests, handle)
    }

    const TAGS_BODY: &str = r#"{"models":[{"name":"llama3:8b","size":4661211808,"details":{"parameter_size":"8B","quantization_level":"Q4_0"}},{"name":"qwen2.5-coder:7b","size":4683087519,"details":{"parameter_size":"7B","quantization_level":"Q4_K_M"}}]}"#;

    #[tokio::test]
    async fn refresh_reads_installed_models_from_the_tags_endpoint() {
        let response = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", TAGS_BODY.len(), TAGS_BODY)
                .into_boxed_str(),
        );
        let (port, handle) = serve_once(response).await;

        let provider = OllamaProvider::new("127.0.0.1", port);
        assert_eq!(provider.refresh().await.unwrap(), 2);

        let request = handle.await.unwrap();
        assert!(request.starts_with("GET /api/tags HTTP/1.1"), "got: {request}");

        let mut names: Vec<String> = provider.list_models().into_iter().map(|m| m.name).collect();
        names.sort();
        assert_eq!(names, vec!["llama3:8b", "qwen2.5-coder:7b"]);

        let llama = provider.list_models().into_iter().find(|m| m.name == "llama3:8b").unwrap();
        assert_eq!(llama.parameter_count_billions, Some(8.0));
        assert_eq!(llama.quantization.as_deref(), Some("Q4_0"));
        assert_eq!(llama.provider, "ollama");
        assert!(llama.local);
        // 4661211808 bytes ≈ 4445 MB, a real figure off the daemon,
        // which is what makes the resource scheduler's budgeting mean
        // something for real models.
        assert_eq!(llama.estimated_vram_mb, 4445);
    }

    #[tokio::test]
    async fn the_same_model_name_always_gets_the_same_id() {
        // Specialists store `preferred_model_ids`; if ids were random
        // per refresh, every restart would silently orphan them.
        assert_eq!(stable_id_for("llama3:8b"), stable_id_for("llama3:8b"));
        assert_ne!(stable_id_for("llama3:8b"), stable_id_for("qwen2.5-coder:7b"));
    }

    #[tokio::test]
    async fn generate_posts_the_right_payload_and_returns_the_daemons_text_and_token_counts() {
        let tags_response = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", TAGS_BODY.len(), TAGS_BODY).into_boxed_str(),
        );
        let (tags_port, tags_handle) = serve_once(tags_response).await;
        let provider = OllamaProvider::new("127.0.0.1", tags_port);
        provider.refresh().await.unwrap();
        tags_handle.await.unwrap();

        let gen_body = r#"{"model":"llama3:8b","response":"fn main() {}","done":true,"eval_count":12,"prompt_eval_count":30}"#;
        let gen_response = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", gen_body.len(), gen_body).into_boxed_str(),
        );
        let (gen_port, gen_handle) = serve_once(gen_response).await;

        // Point the same provider at the generate server, keeping the
        // model map refresh() built.
        let provider = OllamaProvider {
            host: "127.0.0.1".into(),
            port: gen_port,
            models: Mutex::new(provider.models.into_inner().unwrap()),
        };
        let model_id = provider.list_models().into_iter().find(|m| m.name == "llama3:8b").unwrap().id;

        let result = provider
            .generate(GenerationRequest {
                model_id,
                system_prompt: "You are a Rust specialist.".into(),
                prompt: "Write a main function.".into(),
                            max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();

        assert_eq!(result.text, "fn main() {}");
        // Real counts from the daemon, not a character estimate.
        assert_eq!(result.estimated_tokens, 42);

        let request = gen_handle.await.unwrap();
        assert!(request.starts_with("POST /api/generate HTTP/1.1"), "got: {request}");
        assert!(request.contains("Content-Type: application/json"));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let sent: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(sent["model"], "llama3:8b");
        assert_eq!(sent["system"], "You are a Rust specialist.");
        assert_eq!(sent["prompt"], "Write a main function.");
        assert_eq!(sent["stream"], false, "streaming must be off, the trait returns one complete result");
    }

    #[tokio::test]
    async fn generating_with_an_unknown_model_fails_before_touching_the_network() {
        // No server is started at all: if this tried to connect it would
        // fail with a connection error instead of the clear message.
        let provider = OllamaProvider::new("127.0.0.1", 1);
        let err = provider
            .generate(GenerationRequest { model_id: Uuid::new_v4(), system_prompt: String::new(), prompt: "hi".into(), max_tokens: None, temperature: None, })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refresh()"), "should point at the actual fix: {err}");
    }

    #[tokio::test]
    async fn a_daemon_error_surfaces_with_its_own_message_rather_than_a_generic_failure() {
        let body = r#"{"error":"model 'nope' not found, try pulling it first"}"#;
        let response = Box::leak(
            format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_boxed_str(),
        );
        let (port, handle) = serve_once(response).await;

        let provider = OllamaProvider::new("127.0.0.1", port);
        let err = provider.refresh().await.unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("404"), "got: {chain}");
        assert!(chain.contains("try pulling it first"), "the daemon's own guidance must survive: {chain}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_daemon_that_isnt_running_gives_a_connection_error_naming_the_address() {
        // Port 1 is never an Ollama daemon. This is the single most
        // likely real-world failure, the user simply hasn't started
        // Ollama, so the message needs to be recognizable.
        let provider = OllamaProvider::new("127.0.0.1", 1);
        let err = format!("{:#}", provider.refresh().await.unwrap_err());
        assert!(err.contains("127.0.0.1:1"), "got: {err}");
    }

    // ---- round 38: /api/show enrichment (real context length + -------
    // capabilities, replacing round 37's honest "unknown"/"not guessed"
    // placeholders) ------------------------------------------------------

    #[tokio::test]
    async fn refresh_enriches_each_model_with_real_context_length_and_capabilities_from_show() {
        // Real, verbatim shapes from Ollama's own documented /api/show
        // examples, not invented. Note the architecture-prefixed
        // model_info keys (`llama.*` vs `qwen2.*`): this is the exact
        // reason `find_context_length` searches by suffix instead of a
        // fixed key name.
        let llama_show = r#"{"model_info":{"general.architecture":"llama","llama.context_length":8192,"llama.embedding_length":4096},"capabilities":["completion","vision"]}"#;
        let qwen_show = r#"{"model_info":{"general.architecture":"qwen2","qwen2.context_length":32768},"capabilities":["completion","tools"]}"#;

        let mut show_responses = HashMap::new();
        show_responses.insert("llama3:8b", llama_show);
        show_responses.insert("qwen2.5-coder:7b", qwen_show);
        let (port, requests, handle) = serve_ollama(TAGS_BODY, show_responses);

        let provider = OllamaProvider::new("127.0.0.1", port);
        provider.refresh().await.unwrap();
        handle.abort();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|r| r.starts_with("GET /api/tags")));
        assert_eq!(requests.iter().filter(|r| r.starts_with("POST /api/show")).count(), 2, "one /api/show per installed model");

        let llama = provider.list_models().into_iter().find(|m| m.name == "llama3:8b").unwrap();
        assert_eq!(llama.context_capacity_tokens, 8192);
        assert!(!llama.tool_support, "llama's capabilities didn't include tools");
        assert!(llama.modalities.contains(&Modality::Vision), "llama's capabilities included vision");

        let qwen = provider.list_models().into_iter().find(|m| m.name == "qwen2.5-coder:7b").unwrap();
        assert_eq!(qwen.context_capacity_tokens, 32768, "found via the qwen2-prefixed key, not a hardcoded llama one");
        assert!(qwen.tool_support, "qwen's capabilities included tools");
        assert!(!qwen.modalities.contains(&Modality::Vision));
    }

    #[test]
    fn find_context_length_works_across_different_architecture_prefixes() {
        // The actual point of this round: real Ollama installs mix
        // architectures, and each one prefixes model_info differently.
        // A function that only checked "llama.context_length" would
        // silently report 0 for every non-Llama model, exactly the
        // "unknown" placeholder round 38 exists to replace with a real
        // number.
        for (key, value) in [
            ("llama.context_length", 8192u64),
            ("qwen2.context_length", 32768),
            ("gemma3.context_length", 131072),
            ("phi3.context_length", 4096),
        ] {
            let mut info = HashMap::new();
            info.insert(key.to_string(), serde_json::json!(value));
            assert_eq!(find_context_length(&info), value as u32, "failed for {key}");
        }
    }

    #[test]
    fn find_context_length_returns_zero_meaning_unknown_when_absent() {
        // 0 is round 37's own established convention for "unknown",
        // not "the model truly has zero context", worth its own
        // assertion so a future change can't quietly redefine it.
        assert_eq!(find_context_length(&HashMap::new()), 0);
    }

    #[tokio::test]
    async fn refresh_falls_back_to_the_honest_placeholders_when_show_fails_for_one_model() {
        // No show_responses registered at all: every /api/show gets a
        // 404, standing in for a real daemon quirk (a model that
        // doesn't answer /api/show, a transient failure) without
        // pretending the whole refresh should fail over one model.
        let (port, _requests, handle) = serve_ollama(TAGS_BODY, HashMap::new());
        let provider = OllamaProvider::new("127.0.0.1", port);

        // refresh() itself must still succeed...
        let count = provider.refresh().await.unwrap();
        assert_eq!(count, 2);
        handle.abort();

        // ...and every model keeps the honest round-37 placeholders
        // rather than the refresh silently discarding the model or
        // propagating the /api/show error upward.
        for model in provider.list_models() {
            assert_eq!(model.context_capacity_tokens, 0, "{}: expected 'unknown'", model.name);
            assert!(!model.tool_support, "{}: expected 'not guessed'", model.name);
        }
    }

    /// Builds a chunked-encoding HTTP/1.1 response out of `parts`, each
    /// becoming its own HTTP chunk, i.e. its own separate invocation of
    /// `http::request_streaming`'s `on_chunk` callback. Used to prove
    /// `generate_streaming`'s line-reassembly buffer, by handing it a
    /// JSON line deliberately split across two chunks (see below).
    fn chunked_response(parts: &[&str]) -> String {
        let mut body = String::new();
        for part in parts {
            body.push_str(&format!("{:x}\r\n{}\r\n", part.len(), part));
        }
        body.push_str("0\r\n\r\n");
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}")
    }

    /// Refreshes a provider against a throwaway `/api/tags` server, then
    /// rebuilds it pointed at `gen_response` for a `/api/generate` call
    /// the same two-server pattern
    /// `generate_posts_the_right_payload_and_returns_the_daemons_text_and_token_counts`
    /// uses, factored out since every streaming test below needs it too.
    async fn provider_ready_to_generate(gen_response: &'static str) -> (OllamaProvider, Uuid, tokio::task::JoinHandle<String>) {
        let tags_response = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", TAGS_BODY.len(), TAGS_BODY).into_boxed_str(),
        );
        let (tags_port, tags_handle) = serve_once(tags_response).await;
        let provider = OllamaProvider::new("127.0.0.1", tags_port);
        provider.refresh().await.unwrap();
        tags_handle.await.unwrap();

        let (gen_port, gen_handle) = serve_once(gen_response).await;
        let provider = OllamaProvider {
            host: "127.0.0.1".into(),
            port: gen_port,
            models: Mutex::new(provider.models.into_inner().unwrap()),
        };
        let model_id = provider.list_models().into_iter().find(|m| m.name == "llama3:8b").unwrap().id;
        (provider, model_id, gen_handle)
    }

    #[tokio::test]
    async fn generate_streaming_requests_stream_true_and_reassembles_a_line_split_across_chunks() {
        let response = Box::leak(
            chunked_response(&[
                "{\"response\":\"Hello\",\"done\":false}\n",
                // Deliberately split mid-field: this JSON line arrives
                // as two separate `on_chunk` calls, neither of which is
                // valid JSON on its own, proving `generate_streaming`
                // buffers across calls rather than parsing per-call.
                "{\"respon",
                "se\":\" world\",\"done\":false}\n",
                "{\"response\":\"\",\"done\":true,\"eval_count\":42,\"prompt_eval_count\":7}\n",
            ])
            .into_boxed_str(),
        );
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider
            .generate_streaming(
                GenerationRequest {
                    model_id,
                    system_prompt: "You are a Rust specialist.".into(),
                    prompt: "Write a main function.".into(),
                                    max_tokens: None,
                    temperature: None,
                },
                tx,
            )
            .await
            .unwrap();

        assert_eq!(result.text, "Hello world");
        // Real counts from the daemon's `done: true` line, same as generate().
        assert_eq!(result.estimated_tokens, 49);

        let mut received = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            received.push(chunk);
        }
        assert_eq!(
            received,
            vec!["Hello".to_string(), " world".to_string()],
            "expected one channel send per response fragment, reassembled correctly despite the network split"
        );

        let request = gen_handle.await.unwrap();
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let sent: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(sent["stream"], true, "generate_streaming must request stream:true, unlike generate()");
    }

    #[tokio::test]
    async fn generate_streaming_surfaces_ollamas_mid_stream_error_field_as_an_err() {
        let response = Box::leak(
            chunked_response(&["{\"response\":\"partial\",\"done\":false}\n", "{\"error\":\"model crashed\"}\n"])
                .into_boxed_str(),
        );
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = provider
            .generate_streaming(
                GenerationRequest { model_id, system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, },
                tx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("model crashed"), "got: {err}");
        gen_handle.abort();
    }

    #[tokio::test]
    async fn generate_streaming_surfaces_an_unparseable_line_as_an_err_instead_of_silently_truncating() {
        let response = Box::leak(chunked_response(&["not valid json at all\n"]).into_boxed_str());
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = provider
            .generate_streaming(
                GenerationRequest { model_id, system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, },
                tx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("unparseable line"), "got: {err}");
        gen_handle.abort();
    }

    // ---- round 50: generation limits -----------------------------------

    fn request_with(max_tokens: Option<u32>, temperature: Option<f32>) -> GenerationRequest {
        GenerationRequest {
            model_id: Uuid::new_v4(),
            system_prompt: "s".into(),
            prompt: "p".into(),
            max_tokens,
            temperature,
        }
    }

    #[test]
    fn unset_limits_leave_the_generate_payload_byte_identical() {
        let base = serde_json::json!({"model": "m", "prompt": "p", "system": "s", "stream": false});
        let out = with_ollama_options(base.clone(), &request_with(None, None));
        assert_eq!(out, base, "a request that sets nothing must not even gain an empty `options` object");
    }

    #[test]
    fn max_tokens_maps_to_ollamas_num_predict_inside_options() {
        let base = serde_json::json!({"model": "m", "stream": true});
        let out = with_ollama_options(base, &request_with(Some(5), Some(0.5)));
        assert_eq!(out["options"]["num_predict"], 5);
        assert_eq!(out["options"]["temperature"], 0.5);
        assert!(out.get("max_tokens").is_none(), "Ollama's /api/generate doesn't use the OpenAI field name");
        assert_eq!(out["stream"], true, "existing fields must be untouched");

        let only_temp = with_ollama_options(serde_json::json!({}), &request_with(None, Some(0.0)));
        assert!(only_temp["options"].get("num_predict").is_none());
        assert_eq!(only_temp["options"]["temperature"], 0.0, "an explicit temperature of 0 is a real setting, not 'unset'");
    }

    // ---- round 51: finish reason ---------------------------------------

    fn ok_response(body: &str) -> &'static str {
        Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_boxed_str())
    }

    fn plain_request(model_id: Uuid) -> GenerationRequest {
        GenerationRequest { model_id, system_prompt: "s".into(), prompt: "p".into(), max_tokens: Some(3), temperature: None }
    }

    #[tokio::test]
    async fn generate_reports_ollamas_done_reason_as_the_finish_reason() {
        for (reason_json, expected) in [
            (r#","done_reason":"length""#, FinishReason::Length),
            (r#","done_reason":"stop""#, FinishReason::Stop),
            ("", FinishReason::Unknown),
            (r#","done_reason":"something_new""#, FinishReason::Unknown),
        ] {
            let body = format!(r#"{{"response":"cut o","done":true,"eval_count":3,"prompt_eval_count":1{reason_json}}}"#);
            let (provider, model_id, handle) = provider_ready_to_generate(ok_response(&body)).await;
            let result = provider.generate(plain_request(model_id)).await.unwrap();
            handle.await.unwrap();
            assert_eq!(result.finish_reason, expected, "body: {body}");
        }
    }

    #[tokio::test]
    async fn generate_streaming_takes_the_finish_reason_from_the_final_done_line() {
        let response = Box::leak(
            chunked_response(&[
                "{\"response\":\"cut\",\"done\":false}\n",
                "{\"response\":\" o\",\"done\":false}\n",
                "{\"response\":\"\",\"done\":true,\"done_reason\":\"length\",\"eval_count\":3,\"prompt_eval_count\":1}\n",
            ])
            .into_boxed_str(),
        );
        let (provider, model_id, handle) = provider_ready_to_generate(response).await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider.generate_streaming(plain_request(model_id), tx).await.unwrap();
        handle.await.unwrap();
        assert_eq!(result.text, "cut o");
        assert_eq!(result.finish_reason, FinishReason::Length);
    }
}
