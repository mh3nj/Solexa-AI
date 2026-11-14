#!/usr/bin/env python3
"""Round 49: builds a tiny, genuinely valid GGUF model entirely offline,
using llama.cpp's own official `gguf` PyPI package (pip install gguf) --
no network access needed beyond what's already used to install that
package.

## Why this exists

Rounds 37/38/42/43 downloaded and ran real Ollama and llama.cpp
*daemon* binaries from GitHub releases -- an allowed domain -- and
confirmed everything short of actual generation against a real *loaded
model*. That last piece stayed blocked for 13 rounds because every real
GGUF model weight file anyone could find -- toy, tiny, or otherwise --
turned out to live exclusively on Hugging Face, which (like
ollama.com's own registry) this sandbox's network allowlist blocks.

Round 49 checked whether that was actually a dead end before accepting
it as one: `crates.io` (already allowed, since Rust dependencies come
from there) hosts `llama-cpp-4`/`llama-cpp-sys-4`, whose own test
harness pointed at how a valid GGUF is actually structured -- but
didn't bundle one. Rather than stop there, this script builds one
directly using llama.cpp's own official Python writer library, which
*is* on PyPI (`pip install gguf`, PyPI is already allowed). The
resulting model has real, correctly-structured GGUF metadata and
tensors for a tiny 1-layer "llama"-architecture network with a
byte-level tokenizer (so it can tokenize any input by construction) --
loadable and runnable by a real, unmodified `llama-server` or `ollama
create`, for real. Its weights are untrained random floats, so its
*output* is garbage -- that was never the point. The point is
confirming `LlamaCppProvider`/`OllamaProvider`'s `generate()` and
`generate_streaming()` complete a genuine round trip against a real,
loaded model, which nothing before round 49 had ever done.

## Usage

    pip install gguf
    python3 scripts/build_synthetic_test_model.py
    # writes ./tiny.gguf in the current directory

Then, for llama.cpp (router mode, matching `llamacpp_smoke_test.rs`'s
own doc comment):

    mkdir -p /tmp/ggufdir && cp tiny.gguf /tmp/ggufdir/
    llama-server --host 127.0.0.1 --port 8080 --models-dir /tmp/ggufdir
    cargo run -p solexa-models --example llamacpp_smoke_test

Or for Ollama (entirely offline import, no `ollama.com` needed):

    echo "FROM $(pwd)/tiny.gguf" > Modelfile
    ollama serve &
    ollama create tiny-test -f Modelfile
    cargo run -p solexa-models --example ollama_smoke_test

Both smoke tests were extended in round 49 to also exercise
`generate_streaming()`, not just `generate()` -- neither had ever been
run against a real loaded model before.
"""
import gguf
import numpy as np

# Tiny "llama"-architecture model with a byte-level GPT-2 style BPE
# tokenizer (every one of the 256 possible bytes has its own token, one
# harmless merge entry since an empty merges list is rejected at load
# time) so it can tokenize ANY input text by construction -- no risk of
# an incomplete custom vocab failing on real text, unlike an SPM vocab
# built from placeholder pieces. Weights are small random floats
# (untrained) -- this proves real load+generate plumbing end-to-end,
# not text quality.
N_EMBD = 8
N_LAYER = 1
N_HEAD = 2
N_HEAD_KV = 2
N_FF = 16
# Large enough that a real system+user prompt won't overflow it. The daemon
# caps generation at this length, so it is also the longest a single
# unbounded generation can run: 2048 tokens takes ~300 ms. Round 53's
# cancel-in-flight test needs a generation that runs for SECONDS, so it
# builds a second copy with SOLEXA_SYNTH_CTX=65536 (default unchanged).
import os
CTX = int(os.environ.get("SOLEXA_SYNTH_CTX", "2048"))

rng = np.random.default_rng(42)

def rnd(*shape):
    return (rng.standard_normal(shape) * 0.02).astype(np.float32)

def bytes_to_unicode():
    # Canonical GPT-2 byte<->unicode mapping (openai/gpt-2's encoder.py).
    bs = list(range(ord("!"), ord("~") + 1)) + list(range(ord("\xa1"), ord("\xac") + 1)) + list(range(ord("\xae"), ord("\xff") + 1))
    cs = bs[:]
    n = 0
    for b in range(256):
        if b not in bs:
            bs.append(b)
            cs.append(256 + n)
            n += 1
    return dict(zip(bs, [chr(c) for c in cs]))

byte_encoder = bytes_to_unicode()
byte_tokens = [byte_encoder[i] for i in range(256)]

SPECIALS = ["<unk>", "<s>", "</s>"]
tokens = SPECIALS + byte_tokens
VOCAB = len(tokens)
scores = [0.0] * VOCAB
toktypes = [gguf.TokenType.NORMAL] * VOCAB
toktypes[0] = gguf.TokenType.UNKNOWN
toktypes[1] = gguf.TokenType.CONTROL
toktypes[2] = gguf.TokenType.CONTROL

w = gguf.GGUFWriter("tiny.gguf", arch="llama")

w.add_name("tiny-solexa-test")
w.add_context_length(CTX)
w.add_embedding_length(N_EMBD)
w.add_block_count(N_LAYER)
w.add_feed_forward_length(N_FF)
w.add_head_count(N_HEAD)
w.add_head_count_kv(N_HEAD_KV)
w.add_layer_norm_rms_eps(1e-5)
w.add_rope_dimension_count(N_EMBD // N_HEAD)
w.add_file_type(gguf.LlamaFileType.ALL_F32)

w.add_tokenizer_model("gpt2")
w.add_token_list(tokens)
w.add_token_scores(scores)
w.add_token_types(toktypes)
w.add_token_merges([f"{byte_tokens[0]} {byte_tokens[1]}"])  # loader requires a non-empty merges list to exist at all; a single harmless entry satisfies that without meaningfully affecting tokenization
w.add_bos_token_id(1)
w.add_eos_token_id(2)
w.add_unk_token_id(0)
w.add_chat_template(
    "{% for message in messages %}{{ message['role'] }}: {{ message['content'] }}\n{% endfor %}assistant:"
)

head_dim = N_EMBD // N_HEAD

w.add_tensor("token_embd.weight", rnd(VOCAB, N_EMBD))
w.add_tensor("output_norm.weight", np.ones(N_EMBD, dtype=np.float32))
w.add_tensor("output.weight", rnd(VOCAB, N_EMBD))

for i in range(N_LAYER):
    p = f"blk.{i}."
    w.add_tensor(p + "attn_norm.weight", np.ones(N_EMBD, dtype=np.float32))
    w.add_tensor(p + "attn_q.weight", rnd(N_HEAD * head_dim, N_EMBD))
    w.add_tensor(p + "attn_k.weight", rnd(N_HEAD_KV * head_dim, N_EMBD))
    w.add_tensor(p + "attn_v.weight", rnd(N_HEAD_KV * head_dim, N_EMBD))
    w.add_tensor(p + "attn_output.weight", rnd(N_EMBD, N_HEAD * head_dim))
    w.add_tensor(p + "ffn_norm.weight", np.ones(N_EMBD, dtype=np.float32))
    w.add_tensor(p + "ffn_gate.weight", rnd(N_FF, N_EMBD))
    w.add_tensor(p + "ffn_down.weight", rnd(N_EMBD, N_FF))
    w.add_tensor(p + "ffn_up.weight", rnd(N_FF, N_EMBD))

w.write_header_to_file()
w.write_kv_data_to_file()
w.write_tensors_to_file()
w.close()
print(f"wrote tiny.gguf, vocab={VOCAB}")
