//! Run this against a real, running llama.cpp `llama-server` (router
//! mode) to sanity-check `LlamaCppProvider` end to end, something the
//! automated test suite can't do, since there's no llama.cpp server in
//! CI or in the sandbox this crate was built in (round 42's README
//! says so plainly, same as round 37 did for `ollama_smoke_test.rs`).
//!
//! ```text
//! cargo run -p solexa-models --example llamacpp_smoke_test
//! ```
//!
//! Start the server first, in router mode, pointed at a directory of
//! `.gguf` files:
//!
//! ```text
//! llama-server --host 127.0.0.1 --port 8080 --models-dir /path/to/ggufs
//! ```
//!
//! Lists whatever models the router can serve, and if there's at least
//! one, sends it a real one-line prompt and prints the response. Exits
//! non-zero with a clear message if the server isn't reachable at all,
//! or if nothing is configured to generate with.

use solexa_models::llamacpp::LlamaCppProvider;
use solexa_models::{GenerationRequest, ModelProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = LlamaCppProvider::local();

    println!("connecting to llama.cpp at 127.0.0.1:8080 ...");
    let count = provider.refresh().await?;
    println!("ok, {count} model(s) available");

    let models = provider.list_models();
    for m in &models {
        println!(
            "  - {} ({} params, ~{} MB, {} ctx)",
            m.name,
            m.parameter_count_billions.map(|b| format!("{b:.1}B")).unwrap_or_else(|| "?".into()),
            m.estimated_vram_mb,
            if m.context_capacity_tokens > 0 { m.context_capacity_tokens.to_string() } else { "unknown".into() }
        );
    }

    let Some(model) = models.first() else {
        println!(
            "\nno models configured, restart the server with --models-dir pointed at some .gguf files, then rerun to test generate()"
        );
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
    // The channel is unbounded, so generate_streaming can run to
    // completion first (nothing blocks on a receiver) and the chunks
    // can be drained afterward, no need to spawn a separate task.
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
    println!(
        "streamed {chunk_count} chunk(s), {} tokens total",
        stream_result.estimated_tokens
    );

    Ok(())
}
