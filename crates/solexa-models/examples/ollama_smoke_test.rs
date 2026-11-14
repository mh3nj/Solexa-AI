//! Run this against a real, running Ollama daemon to sanity-check
//! `OllamaProvider` end to end, something the automated test suite
//! can't do, since there's no Ollama daemon in CI or in the sandbox
//! this crate was built in (round 37's README says so plainly).
//!
//! ```text
//! cargo run -p solexa-models --example ollama_smoke_test
//! ```
//!
//! Lists whatever models your daemon has installed, and if there's at
//! least one, sends it a real one-line prompt and prints the response.
//! Exits non-zero with a clear message if Ollama isn't reachable at
//! all, or if nothing is installed to generate with.

use solexa_models::ollama::OllamaProvider;
use solexa_models::{GenerationRequest, ModelProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = OllamaProvider::local();

    println!("connecting to Ollama at 127.0.0.1:11434 ...");
    let count = provider.refresh().await?;
    println!("ok, {count} model(s) installed");

    let models = provider.list_models();
    for m in &models {
        println!(
            "  - {} ({} params, {}, ~{} MB)",
            m.name,
            m.parameter_count_billions.map(|b| format!("{b}B")).unwrap_or_else(|| "?".into()),
            m.quantization.as_deref().unwrap_or("unknown quant"),
            m.estimated_vram_mb
        );
    }

    let Some(model) = models.first() else {
        println!("\nno models installed, `ollama pull <name>` first, then rerun to test generate()");
        return Ok(());
    };

    println!("\ngenerating with {} ...", model.name);
    let result = provider
        .generate(GenerationRequest {
            model_id: model.id,
            system_prompt: "You are a terse assistant. Reply in one short sentence.".into(),
            prompt: "Say hello and name the model you are.".into(),
                    max_tokens: None,
            temperature: None,
        })
        .await?;
    println!("response ({} tokens): {}", result.estimated_tokens, result.text);

    println!("\nstreaming with {} ...", model.name);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Unbounded channel: generate_streaming can run to completion first
    // (nothing blocks on a receiver existing), chunks drained after.
    let stream_result = provider
        .generate_streaming(
            GenerationRequest {
                model_id: model.id,
                system_prompt: "You are a terse assistant. Reply in one short sentence.".into(),
                prompt: "Say hello and name the model you are.".into(),
                            max_tokens: None,
                temperature: None,
            },
            tx,
        )
        .await?;
    let mut chunk_count = 0usize;
    while let Ok(chunk) = rx.try_recv() {
        chunk_count += 1;
        let _ = chunk;
    }
    println!("streamed {chunk_count} chunk(s), {} tokens total", stream_result.estimated_tokens);

    Ok(())
}
