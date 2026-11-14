//! Model metadata, capability, not brand.
//!
//! Rule from the spec: "The engine should care about what a model can
//! do, not whether it is called Qwen, Llama, Gemma, Mistral, DeepSeek."
//! `ModelMetadata` is deliberately capability-first: `id`/`name` are for
//! humans, everything else is what the scheduler and capability router
//! actually use.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Vision,
    Embedding,
    Speech,
    ImageGeneration,
    Code,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: Uuid,
    pub name: String,
    /// e.g. "mock", "ollama", "llama.cpp", identifies which
    /// `ModelProvider` adapter (solexa-models) can actually run this
    /// model. Not a hardcoded enum, so new adapters don't require a
    /// domain-crate release.
    pub provider: String,
    pub modalities: Vec<Modality>,
    pub context_capacity_tokens: u32,
    pub parameter_count_billions: Option<f32>,
    pub quantization: Option<String>,
    pub estimated_vram_mb: u32,
    pub estimated_ram_mb: u32,
    pub tool_support: bool,
    pub structured_output_support: bool,
    pub streaming_support: bool,
    pub local: bool,
    pub license: Option<String>,
}
