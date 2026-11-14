//! `LlamaCppProvider`, a second real `ModelProvider` (round 42),
//! alongside `OllamaProvider` (round 37). The whole point of the
//! `ModelProvider` trait (spec section 5, "adapter architecture... do
//! not hard-code the application around one runtime") was always to
//! make this possible without touching the coordinator, the trait
//! itself, or anything downstream, this round is the proof.
//!
//! It speaks llama.cpp's own `llama-server` binary's HTTP API, run in
//! its **router mode** (`--models-dir`), which is new enough that most
//! third-party documentation still describes the older one-model-per-
//! process mode. Router mode is the architecturally right target for
//! SOLEXA: it serves multiple models from one running server and loads
//! them on demand, which is the same "many models, pick one per
//! request" shape `OllamaProvider` and the `ModelProvider` trait
//! already assume, the single-model classic mode would have forced an
//! awkward "one `LlamaCppProvider` instance per running process"
//! design instead.
//!
//! Three endpoints are used:
//! - `GET /v1/models`, OpenAI-shaped list of what the router can
//!   serve, each with a `meta` object carrying real per-model detail
//!   up front (`n_ctx_train`, `n_params`, `size`), unlike Ollama's
//!   `/api/tags`, which needed a second per-model call (round 38's
//!   `/api/show`) to get the same *kind* of detail.
//! - `GET /props?model=<id>&autoload=false`, llama.cpp's own
//!   per-model capability detail (round 43; the second per-model call
//!   this provider needed after all, just for different information
//!   than `/v1/models` already carries): real vision detection and
//!   real tool-calling support, replacing round 42's honest
//!   placeholders. `autoload=false` is load-bearing, not incidental,
//!   see `props`'s own doc comment.
//! - `POST /v1/chat/completions`, OpenAI-compatible chat completion,
//!   in two modes exactly like `OllamaProvider::generate`/
//!   `generate_streaming`: one-shot `"stream": false`, and a real
//!   `"stream": true` using Server-Sent Events (`data: {...}\n\n`,
//!   terminated by `data: [DONE]\n\n`) rather than Ollama's
//!   newline-delimited raw JSON, a different wire format, reassembled
//!   here the same way `ollama.rs` reassembles its own, on top of the
//!   same shared `http::request_streaming`.
//!
//! **Honest capability gap, still open**: `structured_output_support`
//! stays a `false` placeholder, llama.cpp's JSON-mode/grammar-
//! constrained output works via sampling-time constraints on
//! essentially any model, not a distinct per-model capability flag,
//! the identical reasoning `OllamaProvider::refresh` documents for its
//! own `structured_output_support: false`. `tool_support` *was* this
//! kind of gap in round 42, round 43 closed it via `/props`'s
//! `chat_template_caps`, once a genuine documented source for it
//! turned up (see `props`'s own doc comment for where).
//!
//! **Honest testing note, more specific than usual**: this sandbox
//! genuinely downloaded and ran a real `llama-server` binary (v0.4.1,
//! build b11060, from llama.cpp's own GitHub releases) in router mode
//! and confirmed `GET /health` (`{"status":"ok"}`), `GET /v1/models`
//! (`{"data":[],"object":"list"}` with zero models configured),
//! `POST /v1/chat/completions`'s and `GET /props?model=...`'s real
//! error shape for an unknown model (both identical:
//! `{"error":{"code":400,"message":"model 'x' not found",...}}`),
//! genuinely live, not recorded. What's *not* been run live is anything
//! that needs an actual model loaded (`/v1/models` with real entries,
//! `/props`'s success shape with real `chat_template_caps`, an actual
//! completion, actual token streaming), because `--models-dir`
//! needs real `.gguf` files and this sandbox can't reach Hugging Face
//! to get one, the identical limitation `OllamaProvider` has had since
//! round 37, for the identical reason. Every test in this file that
//! needs a populated response runs against a real `tokio` TCP server
//! replying with hand-built payloads shaped to match what was
//! genuinely observed live (the empty-list shape, the error shape) or
//! to llama.cpp's own documented `/v1/models`/streaming-chunk formats
//! where nothing live-observed exists to match instead, each test
//! says which.

use crate::http;
use crate::provider_util::stable_id_for;
use crate::{FinishReason, GenerationRequest, GenerationResult, ModelProvider};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use solexa_domain::{Modality, ModelMetadata, Uuid};
use std::collections::HashMap;
use std::sync::Mutex;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8080;

pub struct LlamaCppProvider {
    host: String,
    port: u16,
    /// SOLEXA identifies models by `Uuid`; llama.cpp identifies them by
    /// the string in `/v1/models`' `id` field (a Hugging Face-style
    /// repo path, e.g. `"unsloth/Qwen3.6-35B-A3B-GGUF"`). Holds the
    /// mapping discovered by `refresh`, so `generate` can translate
    /// back, same role `OllamaProvider.models` plays for Ollama's
    /// name-based identifiers.
    models: Mutex<HashMap<Uuid, LlamaCppModel>>,
}

#[derive(Clone)]
struct LlamaCppModel {
    metadata: ModelMetadata,
    llamacpp_id: String,
}

/// Round 50: adds llama-server's OpenAI-compatible `max_tokens` /
/// `temperature` to a chat-completions payload, only when the request
/// actually set them, so an unset request produces exactly the payload
/// it always did.
fn apply_generation_limits(payload: &mut serde_json::Value, request: &GenerationRequest) {
    if let Some(n) = request.max_tokens {
        payload["max_tokens"] = serde_json::json!(n);
    }
    if let Some(t) = request.temperature {
        payload["temperature"] = serde_json::json!(t);
    }
}

#[derive(Deserialize, Default)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    meta: ModelEntryMeta,
}

/// The subset of `/v1/models`' per-model `meta` object this provider
/// actually reads. Confirmed live (see module doc) that `meta` exists
/// and is an object even with zero models configured; the individual
/// fields below (`n_ctx_train`, `n_params`, `size`) are documented
/// llama.cpp behavior, not live-confirmed against real model data,
/// named honestly rather than left unstated.
#[derive(Deserialize, Default)]
struct ModelEntryMeta {
    #[serde(default)]
    n_ctx_train: Option<u32>,
    #[serde(default)]
    n_params: Option<u64>,
    #[serde(default)]
    size: Option<u64>,
}

/// One Server-Sent-Events data payload from a streaming
/// `/v1/chat/completions` response, once the `"data: "` framing has
/// been stripped (see `generate_streaming`), the standard
/// OpenAI-compatible streaming chunk shape.
#[derive(Deserialize, Default)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
}

#[derive(Deserialize, Default)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    /// Round 51: null on every chunk but the last, which carries
    /// `"stop"` or `"length"`.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// The subset of `GET /props?model=<id>&autoload=false`'s response this
/// provider reads, router-mode's per-model capability detail, the
/// llama.cpp counterpart to `OllamaProvider`'s `/api/show` (round 38).
/// Confirmed real field names from two independent sources describing
/// the same shape (a third-party llama-server integration's own
/// documented capability-detection logic, and an independent writeup
/// of a live `/props` response), not live-observed with real model
/// data in this sandbox (see the module doc's testing note), but not a
/// guess either.
#[derive(Deserialize, Default)]
struct PropsResponse {
    #[serde(default)]
    modalities: PropsModalities,
    #[serde(default)]
    chat_template_caps: ChatTemplateCaps,
}

#[derive(Deserialize, Default)]
struct PropsModalities {
    #[serde(default)]
    vision: bool,
}

#[derive(Deserialize, Default)]
struct ChatTemplateCaps {
    #[serde(default)]
    supports_tools: bool,
    #[serde(default)]
    supports_tool_calls: bool,
}

/// Percent-encodes a query-string *value* per RFC 3986, only
/// characters outside the unreserved set (`A-Za-z0-9-_.~`) are encoded.
/// Needed because a model id is a caller-supplied string that can
/// legitimately contain `&`, `=`, `#`, or a space, any of which would
/// otherwise corrupt the query string it's embedded in (`:` and `/`,
/// the characters router-mode ids actually use in practice, e.g.
/// `"unsloth/Qwen3.6-35B-A3B-GGUF"`, are already safe unencoded in a
/// query component per the spec, but encoding them too changes
/// nothing observable and keeps this function simply correct rather
/// than correct-for-the-cases-seen-so-far).
fn percent_encode_query_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl LlamaCppProvider {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port, models: Mutex::new(HashMap::new()) }
    }

    pub fn local() -> Self {
        Self::new(DEFAULT_HOST, DEFAULT_PORT)
    }

    /// `GET /props?model=<id>&autoload=false` for one model, router
    /// mode's per-model detail `/v1/models` doesn't carry (round 43).
    /// `autoload=false` is deliberate, not incidental: without it, the
    /// router would load the model just to answer a capability check,
    /// exactly the "hidden cost" the spec warns against and exactly
    /// what a real third-party llama-server client (round 43's own
    /// evidence for this endpoint's shape) documents doing for the
    /// identical reason. Returns `Ok(default)` shape info (no vision,
    /// no tool support) is signaled by the caller falling back on
    /// `.unwrap_or_default()`, not by this method swallowing the error
    /// a genuine failure is still surfaced to whoever wants to see it
    /// (`refresh` chooses not to look), the same contract
    /// `OllamaProvider::show` documents for itself.
    async fn props(&self, model_id: &str) -> Result<PropsResponse> {
        let path = format!("/props?model={}&autoload=false", percent_encode_query_value(model_id));
        let body = http::request(&self.host, self.port, &path, None)
            .await
            .with_context(|| format!("getting model properties for {model_id}"))?;
        serde_json::from_slice(&body).context("llama.cpp's /props response wasn't the expected JSON")
    }

    /// Asks the router what it can serve and caches the result, same
    /// role, same "must be called before `list_models` returns
    /// anything, deliberately not lazy" contract as
    /// `OllamaProvider::refresh`.
    pub async fn refresh(&self) -> Result<usize> {
        let body = http::request(&self.host, self.port, "/v1/models", None)
            .await
            .with_context(|| format!("listing models from llama.cpp at {}:{}", self.host, self.port))?;
        let parsed: ModelsResponse =
            serde_json::from_slice(&body).context("llama.cpp's /v1/models response wasn't the expected JSON")?;

        let mut models = HashMap::new();
        for entry in parsed.data {
            let id = stable_id_for(&entry.id);
            let estimated_mb = entry.meta.size.map(|s| (s / 1_048_576).max(1) as u32).unwrap_or(0);

            // Round 43: one `/props` per model for the capability
            // detail `/v1/models` doesn't carry, real vision detection
            // and real tool-calling support, replacing round 42's
            // honest "false = not guessed" placeholders with the
            // router's own answer. Deliberately non-fatal per model,
            // same reasoning as `OllamaProvider::refresh`'s `/api/show`
            // call: one model's `/props` failing falls back to the same
            // honest placeholders rather than failing the whole refresh.
            let props = self.props(&entry.id).await.unwrap_or_default();
            let mut modalities = vec![Modality::Text];
            if props.modalities.vision {
                modalities.push(Modality::Vision);
            }
            // Matches the conjunction a real third-party llama-server
            // client documents using to decide whether to enable tools
            // at all, a template can declare tool-call *syntax*
            // support without the model reliably following it, so both
            // flags are required, not either alone.
            let tool_support = props.chat_template_caps.supports_tools && props.chat_template_caps.supports_tool_calls;

            models.insert(
                id,
                LlamaCppModel {
                    metadata: ModelMetadata {
                        id,
                        name: entry.id.clone(),
                        provider: "llamacpp".into(),
                        modalities,
                        // Real, from `/v1/models`' own `meta.n_ctx_train`
                        // the model's trained max context, the same
                        // kind of value `OllamaProvider` reports (round
                        // 38's `find_context_length`), not necessarily
                        // this server instance's *configured* runtime
                        // window (`--ctx-size`) if that differs. Still
                        // 0 ("unknown") when llama.cpp doesn't report it.
                        context_capacity_tokens: entry.meta.n_ctx_train.unwrap_or(0),
                        parameter_count_billions: entry.meta.n_params.map(|n| n as f32 / 1_000_000_000.0),
                        // `/v1/models` doesn't report quantization,
                        // an honest gap, not a guess.
                        quantization: None,
                        estimated_vram_mb: estimated_mb,
                        estimated_ram_mb: estimated_mb,
                        // Real, from `/props`' `chat_template_caps`,
                        // no longer guessed (round 43).
                        tool_support,
                        // Still an honest placeholder: llama.cpp's
                        // JSON-mode/grammar-constrained output works via
                        // sampling-time constraints on essentially any
                        // model, not a distinct per-model capability
                        // flag, the identical reasoning
                        // `OllamaProvider::refresh` documents for its
                        // own `structured_output_support: false`.
                        structured_output_support: false,
                        streaming_support: true,
                        local: true,
                        license: None,
                    },
                    llamacpp_id: entry.id,
                },
            );
        }
        let count = models.len();
        *self.models.lock().unwrap() = models;
        Ok(count)
    }

    fn chat_payload(model: &LlamaCppModel, request: &GenerationRequest, stream: bool) -> Result<Vec<u8>> {
        let mut payload = serde_json::json!({
            "model": model.llamacpp_id,
            "messages": [
                {"role": "system", "content": request.system_prompt},
                {"role": "user", "content": request.prompt},
            ],
            "stream": stream,
        });
        apply_generation_limits(&mut payload, request);
        Ok(serde_json::to_vec(&payload)?)
    }

    fn resolve(&self, model_id: Uuid) -> Result<LlamaCppModel> {
        self.models
            .lock()
            .unwrap()
            .get(&model_id)
            .cloned()
            .ok_or_else(|| anyhow!("model {model_id} isn't one this llama.cpp server reported, call refresh() first"))
    }
}

#[async_trait]
impl ModelProvider for LlamaCppProvider {
    fn list_models(&self) -> Vec<ModelMetadata> {
        self.models.lock().unwrap().values().map(|m| m.metadata.clone()).collect()
    }

    async fn generate(&self, request: GenerationRequest) -> Result<GenerationResult> {
        let model = self.resolve(request.model_id)?;
        let payload = Self::chat_payload(&model, &request, false)?;

        let body = http::request(&self.host, self.port, "/v1/chat/completions", Some(&payload))
            .await
            .with_context(|| format!("generating with llama.cpp model {}", model.llamacpp_id))?;
        let parsed: ChatCompletionResponse = serde_json::from_slice(&body)
            .context("llama.cpp's /v1/chat/completions response wasn't the expected JSON")?;

        let first_choice = parsed.choices.into_iter().next();
        let finish_reason = FinishReason::from_provider(first_choice.as_ref().and_then(|c| c.finish_reason.as_deref()));
        let text = first_choice.map(|c| c.message.content).unwrap_or_default();
        let usage = parsed.usage.unwrap_or_default();
        let estimated_tokens = if usage.prompt_tokens > 0 || usage.completion_tokens > 0 {
            usage.prompt_tokens + usage.completion_tokens
        } else {
            (text.len() / 4).max(1) as u32
        };

        Ok(GenerationResult { text, estimated_tokens, finish_reason })
    }

    /// Real per-token streaming, same as `OllamaProvider::generate_streaming`
    /// (round 39), but reassembling Server-Sent Events instead of
    /// newline-delimited JSON, since that's llama.cpp's own streaming
    /// wire format for `/v1/chat/completions`.
    ///
    /// No token counts are requested here (`stream_options.include_usage`
    /// would ask llama.cpp for a final usage-bearing chunk, but that's an
    /// opt-in feature this round didn't verify live against a real
    /// model, so it isn't depended on), `estimated_tokens` falls back
    /// to the same character-based estimate `MockLocalProvider` and
    /// `OllamaProvider`'s non-streaming fallback both use.
    async fn generate_streaming(
        &self,
        request: GenerationRequest,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<GenerationResult> {
        let model = self.resolve(request.model_id)?;
        let payload = Self::chat_payload(&model, &request, true)?;

        let mut full_text = String::new();
        let mut line_buf: Vec<u8> = Vec::new();
        let mut stream_error: Option<String> = None;
        let mut done = false;
        let mut raw_finish_reason: Option<String> = None;

        http::request_streaming(&self.host, self.port, "/v1/chat/completions", Some(&payload), |data| {
            line_buf.extend_from_slice(data);
            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = line_buf.drain(..=pos).collect();
                line.pop(); // trailing '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let Ok(text) = std::str::from_utf8(&line) else { continue };
                let text = text.trim();
                let Some(data_str) = text.strip_prefix("data:") else { continue };
                let data_str = data_str.trim();
                if data_str.is_empty() {
                    continue;
                }
                if data_str == "[DONE]" {
                    done = true;
                    continue;
                }
                if done || stream_error.is_some() {
                    // Already terminated or failed on an earlier line
                    // this call, keep draining bytes (can't stop the
                    // underlying read loop from here) but stop acting
                    // on their content, same pattern as ollama.rs.
                    continue;
                }
                match serde_json::from_str::<ChatCompletionChunk>(data_str) {
                    Ok(chunk) => {
                        for choice in chunk.choices {
                            if choice.finish_reason.is_some() {
                                raw_finish_reason = choice.finish_reason.clone();
                            }
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    full_text.push_str(&content);
                                    let _ = chunks.send(content);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        stream_error = Some(format!("unparseable SSE data line from llama.cpp: {e}"));
                    }
                }
            }
        })
        .await
        .with_context(|| format!("generating with llama.cpp model {}", model.llamacpp_id))?;

        if let Some(err) = stream_error {
            bail!("llama.cpp's streaming chat completion failed: {err}");
        }

        Ok(GenerationResult {
            estimated_tokens: (full_text.len() / 4).max(1) as u32,
            finish_reason: FinishReason::from_provider(raw_finish_reason.as_deref()),
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
    /// records the request it received, same pattern `ollama.rs` uses
    /// for the identical reason (a genuine socket, so framing and
    /// headers are actually exercised, not assumed).
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

    /// Builds a chunked-encoding HTTP/1.1 response out of `parts`, each
    /// becoming its own HTTP chunk, i.e. its own separate invocation
    /// of `http::request_streaming`'s `on_chunk` callback. Same role as
    /// `ollama.rs`'s identical helper, for the identical reason: proving
    /// `generate_streaming`'s reassembly buffer works across a network
    /// split, not just on already-whole lines.
    fn chunked_response(parts: &[&str]) -> String {
        let mut body = String::new();
        for part in parts {
            body.push_str(&format!("{:x}\r\n{}\r\n", part.len(), part));
        }
        body.push_str("0\r\n\r\n");
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}")
    }

    // The exact body this round's live server returned for
    // `GET /v1/models` with zero models configured, see the module
    // doc's "honest testing note".
    /// A real, multi-request fake llama.cpp router: `/v1/models` gets
    /// `models_body` every time; `GET /props?model=...&autoload=false`
    /// looks up its response by the (decoded) `model` query parameter,
    /// the same way `ollama.rs`'s `serve_ollama` distinguishes `/api/show`
    /// calls by the `model` field in the request body, `refresh()`
    /// needs this since it's a real N+1 request sequence (one
    /// `/v1/models`, one `/props` per model), which `serve_once` (exactly
    /// one connection, ever) can't stand in for.
    fn serve_llamacpp(
        models_body: &'static str,
        props_responses: HashMap<&'static str, &'static str>,
    ) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let port = std_listener.local_addr().unwrap().port();
        let listener = TcpListener::from_std(std_listener).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let Ok(n) = socket.read(&mut buf).await else { continue };
                if n == 0 {
                    continue;
                }
                let received = String::from_utf8_lossy(&buf[..n]).to_string();

                let response = if received.starts_with("GET /v1/models") {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", models_body.len(), models_body)
                } else if received.starts_with("GET /props") {
                    // Every model name this fake needs to recognize is
                    // known up front (the test's own `props_responses`
                    // keys), so matching on the pre-encoded prefix
                    // avoids needing a real query-string parser here.
                    let matched = props_responses.iter().find(|(name, _)| {
                        received.starts_with(&format!("GET /props?model={}", percent_encode_query_value(name)))
                    });
                    match matched {
                        Some((_, body)) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
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

    const LIVE_EMPTY_MODELS_BODY: &str = r#"{"data":[],"object":"list"}"#;

    // The exact body this round's live server returned for a chat
    // completion request naming a model it doesn't have, see the
    // module doc.
    const LIVE_MODEL_NOT_FOUND_BODY: &str =
        r#"{"error":{"code":400,"message":"model 'nonexistent' not found","type":"invalid_request_error"}}"#;

    #[tokio::test]
    async fn refresh_reads_the_live_confirmed_empty_shape_as_zero_models() {
        let response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", LIVE_EMPTY_MODELS_BODY.len(), LIVE_EMPTY_MODELS_BODY);
        let (port, handle) = serve_once(Box::leak(response.into_boxed_str())).await;
        let provider = LlamaCppProvider::new("127.0.0.1", port);

        let count = provider.refresh().await.unwrap();
        assert_eq!(count, 0);
        assert!(provider.list_models().is_empty());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_extracts_real_metadata_from_a_populated_model_list() {
        // Shaped to llama.cpp's own documented /v1/models fields (see
        // module doc), not live-observed with real data, since no
        // model could be loaded in this sandbox, but this is the
        // documented shape, not a guess.
        let body = r#"{"data":[{"id":"unsloth/Qwen3.6-35B-A3B-GGUF","object":"model","owned_by":"llamacpp","meta":{"vocab_type":2,"n_vocab":248320,"n_ctx_train":262144,"n_embd":2048,"n_params":34660610688,"size":22123538944}}],"object":"list"}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        let (port, handle) = serve_once(Box::leak(response.into_boxed_str())).await;
        let provider = LlamaCppProvider::new("127.0.0.1", port);

        let count = provider.refresh().await.unwrap();
        assert_eq!(count, 1);
        let model = provider.list_models().into_iter().next().unwrap();
        assert_eq!(model.name, "unsloth/Qwen3.6-35B-A3B-GGUF");
        assert_eq!(model.provider, "llamacpp");
        assert_eq!(model.context_capacity_tokens, 262144);
        assert!((model.parameter_count_billions.unwrap() - 34.66).abs() < 0.01);
        assert_eq!(model.estimated_vram_mb, (22123538944u64 / 1_048_576) as u32);
        // `serve_once` answers exactly one request (`/v1/models`) and
        // then the connection is gone, `refresh`'s per-model `/props`
        // call fails, falling back to the honest placeholder. The
        // *success* path (real capability detection) has its own
        // dedicated tests below, using `serve_llamacpp`.
        assert!(!model.tool_support);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_enriches_each_model_with_real_capabilities_from_props() {
        let models_body = r#"{"data":[{"id":"unsloth/Qwen3.6-35B-A3B-GGUF","meta":{"n_ctx_train":262144}}]}"#;
        let props_body = r#"{"model_alias":"unsloth/Qwen3.6-35B-A3B-GGUF","modalities":{"vision":true,"audio":false},"chat_template_caps":{"supports_tools":true,"supports_tool_calls":true}}"#;
        let mut props_responses = HashMap::new();
        props_responses.insert("unsloth/Qwen3.6-35B-A3B-GGUF", props_body);
        let (port, requests, handle) = serve_llamacpp(models_body, props_responses);
        let provider = LlamaCppProvider::new("127.0.0.1", port);

        let count = provider.refresh().await.unwrap();
        assert_eq!(count, 1);
        let model = provider.list_models().into_iter().next().unwrap();
        assert!(model.tool_support, "chat_template_caps reported both flags true");
        assert!(model.modalities.contains(&Modality::Vision));
        assert!(model.modalities.contains(&Modality::Text));

        // Confirms the real request shape, not just the response
        // parsing: a GET with the model name percent-encoded into the
        // query string, and autoload=false so checking capabilities
        // never triggers a load.
        let requests = requests.lock().unwrap();
        assert!(
            requests.iter().any(|r| r.starts_with("GET /props?model=unsloth%2FQwen3.6-35B-A3B-GGUF&autoload=false")),
            "{requests:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn tool_support_requires_both_chat_template_cap_flags_not_either_alone() {
        // Matches a real third-party llama-server client's own
        // documented logic (see `refresh`'s doc comment): a template
        // can declare tool-call syntax support without the model
        // reliably following it, so both flags are required.
        let models_body = r#"{"data":[{"id":"partial-support-model","meta":{}}]}"#;
        let props_body = r#"{"chat_template_caps":{"supports_tools":true,"supports_tool_calls":false}}"#;
        let mut props_responses = HashMap::new();
        props_responses.insert("partial-support-model", props_body);
        let (port, _requests, handle) = serve_llamacpp(models_body, props_responses);
        let provider = LlamaCppProvider::new("127.0.0.1", port);

        provider.refresh().await.unwrap();
        let model = provider.list_models().into_iter().next().unwrap();
        assert!(!model.tool_support, "supports_tools alone isn't enough");
        handle.abort();
    }

    #[tokio::test]
    async fn refresh_falls_back_to_the_honest_placeholders_when_props_fails_for_one_model() {
        let models_body =
            r#"{"data":[{"id":"has-props","meta":{}},{"id":"missing-props","meta":{}}]}"#;
        let props_body = r#"{"chat_template_caps":{"supports_tools":true,"supports_tool_calls":true}}"#;
        let mut props_responses = HashMap::new();
        // Deliberately no entry for "missing-props", the fake 404s it,
        // simulating a transient failure or a model that doesn't answer
        // `/props` the way this provider expects.
        props_responses.insert("has-props", props_body);
        let (port, _requests, handle) = serve_llamacpp(models_body, props_responses);
        let provider = LlamaCppProvider::new("127.0.0.1", port);

        // refresh() itself must still succeed for both models...
        let count = provider.refresh().await.unwrap();
        assert_eq!(count, 2);
        handle.abort();

        // ...one gets real capabilities, the other keeps the honest
        // round-42 placeholder rather than the refresh silently
        // discarding it or propagating the /props error upward.
        let models = provider.list_models();
        let with_props = models.iter().find(|m| m.name == "has-props").unwrap();
        let without_props = models.iter().find(|m| m.name == "missing-props").unwrap();
        assert!(with_props.tool_support);
        assert!(!without_props.tool_support);
    }

    #[tokio::test]
    async fn the_same_model_id_string_always_gets_the_same_uuid_across_refreshes() {
        let body = r#"{"data":[{"id":"llama3.2:3b","meta":{}}]}"#;
        let response1 = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        let (port1, handle1) = serve_once(Box::leak(response1.into_boxed_str())).await;
        let provider = LlamaCppProvider::new("127.0.0.1", port1);
        provider.refresh().await.unwrap();
        let first_id = provider.list_models()[0].id;
        handle1.await.unwrap();

        let response2 = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        let (port2, handle2) = serve_once(Box::leak(response2.into_boxed_str())).await;
        let provider2 = LlamaCppProvider::new("127.0.0.1", port2);
        provider2.refresh().await.unwrap();
        let second_id = provider2.list_models()[0].id;
        handle2.await.unwrap();

        assert_eq!(first_id, second_id);
    }

    #[test]
    fn percent_encoding_leaves_safe_characters_alone_and_encodes_the_rest() {
        assert_eq!(percent_encode_query_value("llama3.2:3b"), "llama3.2%3A3b");
        assert_eq!(
            percent_encode_query_value("unsloth/Qwen3.6-35B-A3B-GGUF"),
            "unsloth%2FQwen3.6-35B-A3B-GGUF"
        );
        assert_eq!(percent_encode_query_value("a&b=c"), "a%26b%3Dc");
    }

    #[tokio::test]
    async fn generating_with_an_unknown_model_fails_before_touching_the_network() {
        let provider = LlamaCppProvider::new("127.0.0.1", 1);
        let err = provider
            .generate(GenerationRequest { model_id: Uuid::new_v4(), system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("isn't one this llama.cpp server reported"));
    }

    async fn provider_ready_to_generate(gen_response: &'static str) -> (LlamaCppProvider, Uuid, tokio::task::JoinHandle<String>) {
        let tags_body = r#"{"data":[{"id":"llama3.2:3b","meta":{"n_ctx_train":8192}}]}"#;
        let tags_response =
            Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", tags_body.len(), tags_body).into_boxed_str());
        let (tags_port, tags_handle) = serve_once(tags_response).await;
        let provider = LlamaCppProvider::new("127.0.0.1", tags_port);
        provider.refresh().await.unwrap();
        tags_handle.await.unwrap();

        let (gen_port, gen_handle) = serve_once(gen_response).await;
        let provider = LlamaCppProvider { host: "127.0.0.1".into(), port: gen_port, models: Mutex::new(provider.models.into_inner().unwrap()) };
        let model_id = provider.list_models()[0].id;
        (provider, model_id, gen_handle)
    }

    #[tokio::test]
    async fn generate_posts_stream_false_and_parses_choices_and_usage() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Sure, here is a plan."}}],"usage":{"prompt_tokens":12,"completion_tokens":6}}"#;
        let response = Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_boxed_str());
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let result = provider
            .generate(GenerationRequest { model_id, system_prompt: "You are helpful.".into(), prompt: "Plan it.".into(), max_tokens: None, temperature: None, })
            .await
            .unwrap();

        assert_eq!(result.text, "Sure, here is a plan.");
        assert_eq!(result.estimated_tokens, 18);

        let request = gen_handle.await.unwrap();
        let sent_body = request.split("\r\n\r\n").nth(1).unwrap();
        let sent: serde_json::Value = serde_json::from_str(sent_body).unwrap();
        assert_eq!(sent["model"], "llama3.2:3b");
        assert_eq!(sent["stream"], false);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][1]["content"], "Plan it.");
    }

    #[tokio::test]
    async fn generate_surfaces_the_live_confirmed_error_shape_as_a_readable_error() {
        let response = Box::leak(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{}",
                LIVE_MODEL_NOT_FOUND_BODY.len(),
                LIVE_MODEL_NOT_FOUND_BODY
            )
            .into_boxed_str(),
        );
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let err = provider
            .generate(GenerationRequest { model_id, system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, })
            .await
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("model 'nonexistent' not found"), "got: {err}");
        gen_handle.abort();
    }

    #[tokio::test]
    async fn generate_streaming_reassembles_sse_chunks_split_across_the_network_and_stops_at_done() {
        let response = Box::leak(
            chunked_response(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"Sure\"}}]}\n\n",
                // Split mid-line, same idea as ollama.rs's split-JSON-line
                // test: neither half is parseable alone.
                "data: {\"choices\":[{\"del",
                "ta\":{\"content\":\", here\"}}]}\n\n",
                "data: [DONE]\n\n",
                // Anything after [DONE] must be ignored, not appended.
                "data: {\"choices\":[{\"delta\":{\"content\":\" ignored\"}}]}\n\n",
            ])
            .into_boxed_str(),
        );
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider
            .generate_streaming(
                GenerationRequest { model_id, system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, },
                tx,
            )
            .await
            .unwrap();

        assert_eq!(result.text, "Sure, here");
        let mut received = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            received.push(chunk);
        }
        assert_eq!(received, vec!["Sure".to_string(), ", here".to_string()]);

        let request = gen_handle.await.unwrap();
        let sent_body = request.split("\r\n\r\n").nth(1).unwrap();
        let sent: serde_json::Value = serde_json::from_str(sent_body).unwrap();
        assert_eq!(sent["stream"], true, "generate_streaming must request stream:true, unlike generate()");
    }

    #[tokio::test]
    async fn generate_streaming_surfaces_an_unparseable_data_line_as_an_err() {
        let response = Box::leak(chunked_response(&["data: not valid json at all\n\n"]).into_boxed_str());
        let (provider, model_id, gen_handle) = provider_ready_to_generate(response).await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = provider
            .generate_streaming(GenerationRequest { model_id, system_prompt: "".into(), prompt: "".into(), max_tokens: None, temperature: None, }, tx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unparseable SSE data line"), "got: {err}");
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
    fn unset_limits_leave_the_chat_payload_byte_identical() {
        let base = serde_json::json!({"model": "m", "messages": [], "stream": false});
        let mut payload = base.clone();
        apply_generation_limits(&mut payload, &request_with(None, None));
        assert_eq!(payload, base, "a request that sets nothing must not change the payload at all");
    }

    #[test]
    fn set_limits_become_openai_compatible_fields() {
        let mut payload = serde_json::json!({"model": "m", "stream": true});
        apply_generation_limits(&mut payload, &request_with(Some(64), Some(0.25)));
        assert_eq!(payload["max_tokens"], 64);
        assert_eq!(payload["temperature"], 0.25);
        assert_eq!(payload["stream"], true, "existing fields must be untouched");

        let mut only_temp = serde_json::json!({"model": "m"});
        apply_generation_limits(&mut only_temp, &request_with(None, Some(0.0)));
        assert!(only_temp.get("max_tokens").is_none());
        assert_eq!(only_temp["temperature"], 0.0, "an explicit temperature of 0 is a real setting, not 'unset'");
    }

    // ---- round 51: finish reason ---------------------------------------

    fn limited_request(model_id: Uuid) -> GenerationRequest {
        GenerationRequest { model_id, system_prompt: "s".into(), prompt: "p".into(), max_tokens: Some(3), temperature: None }
    }

    #[tokio::test]
    async fn generate_reports_the_choices_finish_reason() {
        for (reason_json, expected) in [
            (r#","finish_reason":"length""#, FinishReason::Length),
            (r#","finish_reason":"stop""#, FinishReason::Stop),
            (r#","finish_reason":null"#, FinishReason::Unknown),
            ("", FinishReason::Unknown),
        ] {
            let body = format!(r#"{{"choices":[{{"message":{{"role":"assistant","content":"cut o"}}{reason_json}}}],"usage":{{"prompt_tokens":1,"completion_tokens":3}}}}"#);
            let response = Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_boxed_str());
            let (provider, model_id, handle) = provider_ready_to_generate(response).await;
            let result = provider.generate(limited_request(model_id)).await.unwrap();
            handle.await.unwrap();
            assert_eq!(result.finish_reason, expected, "body: {body}");
        }
    }

    #[tokio::test]
    async fn generate_streaming_keeps_the_finish_reason_from_the_last_content_chunk() {
        // llama.cpp sends `finish_reason: null` on every chunk but the
        // last. Nulls before the real value mustn't hide it, and (defensively)
        // a null after it mustn't erase it.
        let response = Box::leak(
            chunked_response(&[
                "data: {\"choices\":[{\"delta\":{\"content\":\"cut\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\" o\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                // A trailing chunk with a null reason must not erase the
                // real one already seen (the property the mutation check
                // for this test exercises).
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}]}\n\n",
                "data: [DONE]\n\n",
            ])
            .into_boxed_str(),
        );
        let (provider, model_id, handle) = provider_ready_to_generate(response).await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider.generate_streaming(limited_request(model_id), tx).await.unwrap();
        handle.await.unwrap();
        assert_eq!(result.text, "cut o");
        assert_eq!(result.finish_reason, FinishReason::Length);
    }
}
