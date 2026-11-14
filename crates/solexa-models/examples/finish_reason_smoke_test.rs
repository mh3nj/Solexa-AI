//! Round 51: proves `GenerationResult::finish_reason` is populated
//! correctly by a REAL running daemon, on both the one-shot and the
//! streaming path, the unit tests only prove we parse the wire format we
//! *assumed* each daemon uses.
//!
//! Usage (daemon already running with the synthetic model loaded, see
//! `scripts/build_synthetic_test_model.py` and the other smoke tests):
//!
//!   cargo run -p solexa-models --example finish_reason_smoke_test -- ollama
//!   cargo run -p solexa-models --example finish_reason_smoke_test -- llamacpp
//!
//! What can and can't be asserted: a run cut off by `max_tokens` MUST
//! report `Length`, that is the whole point and is asserted on every
//! path. A *natural* stop (`Stop`) is not assertable here: the synthetic
//! model has random weights, so it has no reason to ever emit an
//! end-of-sequence token. The unbounded run's reason is printed for the
//! record but deliberately not asserted.

use solexa_models::llamacpp::LlamaCppProvider;
use solexa_models::ollama::OllamaProvider;
use solexa_models::{FinishReason, GenerationRequest, GenerationResult, ModelProvider};

fn request(model_id: solexa_domain::Uuid, max_tokens: Option<u32>) -> GenerationRequest {
    GenerationRequest {
        model_id,
        system_prompt: "You are a terse assistant.".into(),
        prompt: "Say hello.".into(),
        max_tokens,
        temperature: Some(0.0),
    }
}

async fn one_shot(p: &dyn ModelProvider, id: solexa_domain::Uuid, max: Option<u32>) -> anyhow::Result<GenerationResult> {
    p.generate(request(id, max)).await
}

async fn streamed(p: &dyn ModelProvider, id: solexa_domain::Uuid, max: Option<u32>) -> anyhow::Result<GenerationResult> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = p.generate_streaming(request(id, max), tx).await?;
    let mut chunks = 0;
    while rx.try_recv().is_ok() {
        chunks += 1;
    }
    println!("    (streamed {chunks} chunk(s))");
    Ok(result)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let which = std::env::args().nth(1).unwrap_or_default();
    let (provider, model_id): (Box<dyn ModelProvider>, _) = match which.as_str() {
        "ollama" => {
            let p = OllamaProvider::local();
            p.refresh().await?;
            let id = p.list_models().first().ok_or_else(|| anyhow::anyhow!("no Ollama models loaded"))?.id;
            (Box::new(p), id)
        }
        "llamacpp" => {
            let p = LlamaCppProvider::local();
            p.refresh().await?;
            let id = p.list_models().first().ok_or_else(|| anyhow::anyhow!("no llama.cpp models loaded"))?.id;
            (Box::new(p), id)
        }
        other => anyhow::bail!("usage: finish_reason_smoke_test -- <ollama|llamacpp> (got {other:?})"),
    };

    let mut failures = Vec::new();
    let mut check = |name: &str, ok: bool| {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(name.to_string());
        }
    };

    for (path, label) in [(false, "one-shot generate()"), (true, "generate_streaming()")] {
        println!("-- {label}");
        for max in [1u32, 5, 20] {
            let r = if path { streamed(provider.as_ref(), model_id, Some(max)).await? } else { one_shot(provider.as_ref(), model_id, Some(max)).await? };
            println!("    max_tokens={max}: {} chars, finish_reason={:?}", r.text.chars().count(), r.finish_reason);
            check(&format!("{label}: max_tokens={max} reports Length"), r.finish_reason == FinishReason::Length);
        }
        // For the record only, see the module docs.
        let unbounded = if path { streamed(provider.as_ref(), model_id, None).await } else { one_shot(provider.as_ref(), model_id, None).await };
        match unbounded {
            Ok(r) => println!("    unbounded    : {} chars, finish_reason={:?} (not asserted)", r.text.chars().count(), r.finish_reason),
            Err(e) => println!("    unbounded    : daemon error, not asserted ({e:#})"),
        }
    }

    if failures.is_empty() {
        println!("\nALL CHECKS PASSED for {which}");
        Ok(())
    } else {
        anyhow::bail!("{} check(s) failed: {failures:?}", failures.len())
    }
}
