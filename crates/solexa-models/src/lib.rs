//! solexa-models, the model-provider abstraction (spec: "The core
//! should not directly depend on one model runtime").
//!
//! `ModelProvider` is the seam every local runtime (llama.cpp, Ollama,
//! LM Studio, vLLM, ...) plugs into. Phase 1 ships exactly one concrete
//! implementation, `MockLocalProvider`, which does deterministic
//! template-based "generation" instead of real inference.
//!
//! That's a deliberate scope decision, not a placeholder pretending to
//! be finished: this sandbox has no GPU, no model weights, and no
//! network path to a local inference server, so a real adapter (e.g.
//! `OllamaProvider` speaking to `http://localhost:11434`) can't actually
//! be exercised here. What Phase 1 needs to prove, planner → capability
//! matching → context packaging → model invocation → verification, all
//! observable, doesn't depend on the provider being "real"; it depends
//! on the *interface* being one a real provider could implement without
//! the coordinator changing. Swapping `MockLocalProvider` for a real
//! adapter later is a one-file change.

pub mod http;
pub mod llamacpp;
pub mod ollama;
mod provider_util;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solexa_domain::{ModelMetadata, Uuid};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub model_id: Uuid,
    pub system_prompt: String,
    pub prompt: String,
    /// Round 50: an upper bound on how many tokens the model may
    /// generate for this call. `None` (every caller before this round,
    /// and the main task-execution call still) means "no limit
    /// requested", the provider's own default applies, and the wire
    /// payload is byte-identical to what it was before this field
    /// existed. Honored by `OllamaProvider` (`options.num_predict`) and
    /// `LlamaCppProvider` (`max_tokens`); verified against real running
    /// daemons, not only mocks (see `examples/generation_limits_smoke_test.rs`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Round 50: sampling temperature; `None` leaves the provider's
    /// default untouched, same convention as `max_tokens`.
    #[serde(default)]
    pub temperature: Option<f32>,
}

/// Round 51: why a generation stopped, as reported by the provider.
///
/// The one distinction callers actually need is `Length`, the model was
/// cut off by a token limit (`GenerationRequest::max_tokens` or the
/// runtime's own context ceiling) rather than finishing on its own. Output
/// that ended that way is *incomplete*, and a caller that parses it for a
/// structured answer (the review verdict) must not treat the missing
/// answer as a real one.
///
/// `Unknown` is the honest value for "the provider didn't say", the mock
/// providers, an older daemon, or a reason string we don't recognize. It
/// is never treated as truncation, so behavior for those is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished on its own (natural end or a stop sequence).
    Stop,
    /// Cut off by a token limit, the output is incomplete.
    Length,
    /// Not reported, or not a reason this crate recognizes.
    #[default]
    Unknown,
}

impl FinishReason {
    /// Maps a provider's raw finish-reason string. Ollama's `done_reason`
    /// and llama.cpp's OpenAI-style `finish_reason` both use `"stop"` and
    /// `"length"`; anything else (including `None`) is `Unknown` rather
    /// than guessed at.
    pub fn from_provider(raw: Option<&str>) -> Self {
        match raw {
            Some("stop") => FinishReason::Stop,
            Some("length") => FinishReason::Length,
            _ => FinishReason::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationResult {
    pub text: String,
    /// Rough token-count estimate, so the resource-aware scheduler
    /// (Phase 3) has something to work with even before real runtimes
    /// report usage.
    pub estimated_tokens: u32,
    /// Round 51: why generation stopped, see [`FinishReason`].
    #[serde(default)]
    pub finish_reason: FinishReason,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Models this provider can currently serve.
    fn list_models(&self) -> Vec<ModelMetadata>;

    async fn generate(&self, request: GenerationRequest) -> anyhow::Result<GenerationResult>;

    /// Streaming variant of `generate` (round 39): sends partial output
    /// through `chunks` as it becomes available, in addition to
    /// eventually returning the same `GenerationResult` a call to
    /// `generate` would produce.
    ///
    /// The default implementation here is a legitimate, honest
    /// scaled-down behavior for a provider with no real partial
    /// output, not a stub standing in for a "real" implementation
    /// every provider is expected to override. It runs the whole
    /// generation via `generate`, then sends the complete text through
    /// `chunks` as a single chunk before returning. That makes every
    /// existing `ModelProvider` (`MockLocalProvider` included)
    /// streaming-shaped for free, without claiming token-level
    /// granularity it doesn't have, the same honesty standard the
    /// mock providers already hold elsewhere in this file.
    /// `OllamaProvider` is the one override with real per-chunk
    /// behavior (see `ollama.rs`).
    ///
    /// `chunks` closing (the caller dropping its receiver) is not an
    /// error, it just means nobody's listening for partial output
    /// this time; `send`'s `Result` is intentionally ignored here and
    /// should be by overrides too.
    async fn generate_streaming(
        &self,
        request: GenerationRequest,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> anyhow::Result<GenerationResult> {
        let result = self.generate(request).await?;
        let _ = chunks.send(result.text.clone());
        Ok(result)
    }
}

/// A deterministic stand-in for a real local runtime. It doesn't call
/// out to any model, it builds a plausible, inspectable response from
/// the prompt so the rest of the pipeline (verification, events,
/// storage, UI trace) has something real to operate on end-to-end.
pub struct MockLocalProvider {
    models: HashMap<Uuid, ModelMetadata>,
}

impl MockLocalProvider {
    pub fn new(models: Vec<ModelMetadata>) -> Self {
        Self {
            models: models.into_iter().map(|m| (m.id, m)).collect(),
        }
    }
}

#[async_trait]
impl ModelProvider for MockLocalProvider {
    fn list_models(&self) -> Vec<ModelMetadata> {
        self.models.values().cloned().collect()
    }

    async fn generate(&self, request: GenerationRequest) -> anyhow::Result<GenerationResult> {
        let model = self
            .models
            .get(&request.model_id)
            .ok_or_else(|| anyhow::anyhow!("MockLocalProvider has no model {}", request.model_id))?;

        // Deliberately simple and deterministic: acknowledge the task,
        // echo back what it understood, and note what it would need a
        // real model to actually do. This keeps the mock honest about
        // being a mock, rather than fabricating a confident-sounding
        // fake answer. Including `system_prompt` (not just the model
        // name and the raw prompt) matters beyond realism: it's what
        // makes two different specialists sharing one model produce
        // genuinely different text for the same task, the scenario
        // Phase 5's conflict-detection path needs to be able to trigger.
        let text = format!(
            "[{model_name}] ({system_prompt})\nReceived task:\n\"{prompt}\"\n\n\
             This is a mock response, no real inference ran. \
             A real local model (e.g. via an Ollama or llama.cpp adapter \
             implementing ModelProvider) would replace this text while \
             every other part of the pipeline stays the same.",
            model_name = model.name,
            system_prompt = request.system_prompt.trim(),
            prompt = request.prompt.trim(),
        );
        let estimated_tokens = (text.split_whitespace().count() as u32).max(1);

        Ok(GenerationResult {
            text,
            estimated_tokens,
            finish_reason: FinishReason::Unknown,
        })
    }
}

/// A deterministic stand-in for a *reviewing* model, used only when a
/// specialist's `EscalationPolicy.requires_model_review` is set. Real
/// model review means asking an independent model "does this output
/// actually look right?"; this mock can't exercise real judgment any
/// more than `MockLocalProvider` can do real generation, so its
/// "judgment" is a single detectable marker instead: it rejects
/// reviewed content containing the literal substring `REVIEW_REJECT`
/// (case-insensitive) and approves everything else. That's enough to
/// prove the review pipeline's wiring, parsing a verdict out of a
/// model response and acting on it in both directions, actually
/// works, which is what this stand-in exists for. It is never meant to
/// imply real content judgment, and is documented as such everywhere
/// it's used.
pub struct MockReviewerProvider {
    models: HashMap<Uuid, ModelMetadata>,
}

impl MockReviewerProvider {
    pub fn new(models: Vec<ModelMetadata>) -> Self {
        Self {
            models: models.into_iter().map(|m| (m.id, m)).collect(),
        }
    }
}

#[async_trait]
impl ModelProvider for MockReviewerProvider {
    fn list_models(&self) -> Vec<ModelMetadata> {
        self.models.values().cloned().collect()
    }

    async fn generate(&self, request: GenerationRequest) -> anyhow::Result<GenerationResult> {
        let rejected = request.prompt.to_uppercase().contains("REVIEW_REJECT");
        let text = if rejected {
            "VERDICT: REJECTED\nReason: reviewed content contains a flagged marker.".to_string()
        } else {
            "VERDICT: APPROVED\nReason: no issues detected by this mock reviewer.".to_string()
        };
        Ok(GenerationResult {
            text,
            estimated_tokens: 12,
            finish_reason: FinishReason::Unknown,
        })
    }
}

/// Simple in-process registry mapping a model id to the provider that
/// can serve it. The coordinator uses this to go from
/// "specialist prefers model X" to an actual `ModelProvider` call
/// without knowing which runtime backs X.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn ModelProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) {
        self.providers.push(provider);
    }

    pub fn provider_for(&self, model_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        self.providers
            .iter()
            .find(|p| p.list_models().iter().any(|m| m.id == model_id))
            .cloned()
    }

    pub fn all_models(&self) -> Vec<ModelMetadata> {
        self.providers.iter().flat_map(|p| p.list_models()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::Modality;

    fn sample_model() -> ModelMetadata {
        ModelMetadata {
            id: Uuid::new_v4(),
            name: "mock-general-7b".into(),
            provider: "mock".into(),
            modalities: vec![Modality::Text],
            context_capacity_tokens: 8192,
            parameter_count_billions: Some(7.0),
            quantization: Some("Q4_K_M".into()),
            estimated_vram_mb: 6000,
            estimated_ram_mb: 8000,
            tool_support: false,
            structured_output_support: false,
            streaming_support: false,
            local: true,
            license: Some("apache-2.0".into()),
        }
    }

    #[tokio::test]
    async fn mock_provider_generates_deterministic_text() {
        let model = sample_model();
        let model_id = model.id;
        let provider = MockLocalProvider::new(vec![model]);

        let result = provider
            .generate(GenerationRequest {
                model_id,
                system_prompt: "You are helpful.".into(),
                prompt: "Summarize this file.".into(),
                            max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();

        assert!(result.text.contains("mock-general-7b"));
        assert!(result.text.contains("Summarize this file."));
        assert!(result.estimated_tokens > 0);
    }

    #[tokio::test]
    async fn default_generate_streaming_sends_the_whole_text_as_one_honest_chunk() {
        let model = sample_model();
        let model_id = model.id;
        let provider = MockLocalProvider::new(vec![model]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider
            .generate_streaming(
                GenerationRequest {
                    model_id,
                    system_prompt: "You are helpful.".into(),
                    prompt: "Summarize this file.".into(),
                                    max_tokens: None,
                    temperature: None,
                },
                tx,
            )
            .await
            .unwrap();

        // Exactly one chunk, MockLocalProvider has no real partial
        // output to send progressively, and the default trait
        // implementation doesn't pretend otherwise by splitting the
        // text up artificially.
        let chunk = rx.recv().await.unwrap();
        assert_eq!(chunk, result.text);
        assert!(rx.recv().await.is_none(), "expected exactly one chunk, the sender should now be dropped");
    }

    #[test]
    fn registry_finds_provider_by_model_id() {
        let model = sample_model();
        let model_id = model.id;
        let provider: Arc<dyn ModelProvider> = Arc::new(MockLocalProvider::new(vec![model]));

        let mut registry = ProviderRegistry::new();
        registry.register(provider);

        assert!(registry.provider_for(model_id).is_some());
        assert!(registry.provider_for(Uuid::new_v4()).is_none());
    }

    #[tokio::test]
    async fn mock_reviewer_approves_by_default_and_rejects_the_flagged_marker() {
        let model = sample_model();
        let model_id = model.id;
        let reviewer = MockReviewerProvider::new(vec![model]);

        let approved = reviewer
            .generate(GenerationRequest {
                model_id,
                system_prompt: "review this".into(),
                prompt: "This is a perfectly ordinary specialist result.".into(),
                            max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();
        assert!(approved.text.contains("APPROVED"));

        let rejected = reviewer
            .generate(GenerationRequest {
                model_id,
                system_prompt: "review this".into(),
                prompt: "This result contains review_reject somewhere in it.".into(),
                            max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();
        assert!(rejected.text.contains("REJECTED"));
    }

    #[test]
    fn finish_reason_maps_only_the_two_known_strings_and_defaults_to_unknown() {
        assert_eq!(FinishReason::from_provider(Some("stop")), FinishReason::Stop);
        assert_eq!(FinishReason::from_provider(Some("length")), FinishReason::Length);
        assert_eq!(FinishReason::from_provider(Some("tool_calls")), FinishReason::Unknown);
        assert_eq!(FinishReason::from_provider(Some("")), FinishReason::Unknown);
        assert_eq!(FinishReason::from_provider(None), FinishReason::Unknown);
    }

    #[test]
    fn a_result_serialized_before_round_51_still_deserializes_with_unknown() {
        let old = r#"{"text":"hi","estimated_tokens":3}"#;
        let parsed: GenerationResult = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.finish_reason, FinishReason::Unknown);
    }
}
