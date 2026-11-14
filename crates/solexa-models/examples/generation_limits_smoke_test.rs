//! Round 50: proves `GenerationRequest::max_tokens`/`temperature` are
//! honored by a REAL running daemon, not just that the payload looks
//! right in a unit test.
//!
//! Usage (daemon already running with the synthetic model loaded, see
//! `scripts/build_synthetic_test_model.py` and the other smoke tests):
//!
//!   cargo run -p solexa-models --example generation_limits_smoke_test -- ollama
//!   cargo run -p solexa-models --example generation_limits_smoke_test -- llamacpp
//!
//! The check is deliberately strict. At `temperature: 0` generation is
//! deterministic, so a run bounded by `max_tokens` must be an exact
//! *prefix* of the unbounded run, which proves the limit truncates the
//! output rather than changing it, and that `temperature` is honored too
//! (a sampled run would diverge). The synthetic model's weights are
//! random, so the text is gibberish; only the shape matters.

use solexa_models::llamacpp::LlamaCppProvider;
use solexa_models::ollama::OllamaProvider;
use solexa_models::{GenerationRequest, ModelProvider};

async fn run(provider: &dyn ModelProvider, model_id: solexa_domain::Uuid, max_tokens: Option<u32>) -> anyhow::Result<String> {
    Ok(provider
        .generate(GenerationRequest {
            model_id,
            system_prompt: "You are a terse assistant.".into(),
            prompt: "Say hello.".into(),
            max_tokens,
            temperature: Some(0.0),
        })
        .await?
        .text)
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
        other => anyhow::bail!("usage: generation_limits_smoke_test -- <ollama|llamacpp> (got {other:?})"),
    };

    // The baseline is "no limit requested". Real finding from the first
    // live run: at temperature 0 the synthetic model's random weights
    // fall into a repetition loop, llama-server lets it run (~2000
    // chars) but Ollama's own runner aborts it with `HTTP 500:
    // prediction aborted, token repeat limit reached`. That's Ollama
    // guarding against a degenerate loop, not a bug in `max_tokens`,
    // and a small argument for having the limit at all. So if the
    // unbounded baseline errors that way, fall back to a long-but-
    // bounded reference (max_tokens=50) and say so, rather than
    // pretending the unbounded run worked.
    let (reference_label, reference, reference_again) = match run(provider.as_ref(), model_id, None).await {
        Ok(first) => ("unbounded", first, run(provider.as_ref(), model_id, None).await?),
        Err(e) if format!("{e:#}").contains("repeat limit") => {
            println!("note: the unbounded greedy run was aborted by the daemon's own repetition guard ({e:#});");
            println!("      using max_tokens=50 as the long reference instead.");
            ("max_tokens=50", run(provider.as_ref(), model_id, Some(50)).await?, run(provider.as_ref(), model_id, Some(50)).await?)
        }
        Err(e) => return Err(e),
    };
    let five = run(provider.as_ref(), model_id, Some(5)).await?;
    let one = run(provider.as_ref(), model_id, Some(1)).await?;

    println!("{reference_label:<13}: {} chars", reference.chars().count());
    println!("max_tokens=5 : {} chars", five.chars().count());
    println!("max_tokens=1 : {} chars", one.chars().count());

    let mut failures = Vec::new();
    let mut check = |name: &str, ok: bool| {
        println!("{} {name}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(name.to_string());
        }
    };
    check("temperature 0 is deterministic (two identical reference runs match)", reference == reference_again);
    check("the reference run is long enough to make the comparison meaningful (>20 chars)", reference.chars().count() > 20);
    check("max_tokens=5 output is strictly shorter than the reference", five.chars().count() < reference.chars().count());
    check("max_tokens=5 output is an exact prefix of the reference output", reference.starts_with(&five));
    check("max_tokens=1 output is no longer than max_tokens=5 output", one.chars().count() <= five.chars().count());
    check("max_tokens=1 output is an exact prefix of the max_tokens=5 output", five.starts_with(&one));

    if failures.is_empty() {
        println!("\nALL CHECKS PASSED for {which}");
        Ok(())
    } else {
        anyhow::bail!("{} check(s) failed: {failures:?}", failures.len())
    }
}
