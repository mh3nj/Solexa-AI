//! Small helpers shared by every real `ModelProvider` (round 42
//! factored `stable_id_for` out of `ollama.rs`, where it was the only
//! one, once `llamacpp.rs` needed the identical thing).

use solexa_domain::Uuid;

/// Deterministic UUIDv5-style derivation from a provider-native model
/// name/id (Ollama's `"llama3:8b"`, llama.cpp's `/v1/models` `id`).
/// Hand-rolled rather than pulling in uuid's `v5` feature for one call:
/// any stable name→id function works, and this one is obviously
/// stable. Every real provider needs this for the same reason: SOLEXA
/// identifies models by `Uuid`, the provider identifies them by name,
/// and that mapping has to survive an app restart or a
/// `Specialist.preferred_model_ids` entry goes stale the moment the
/// provider re-`refresh()`es.
pub(crate) fn stable_id_for(name: &str) -> Uuid {
    let mut bytes = [0u8; 16];
    // FNV-1a over the name, spread across the 16 bytes by re-hashing
    // with a per-chunk salt. Collisions across a handful of locally
    // installed models are not a realistic concern.
    for (chunk, slot) in bytes.chunks_mut(8).enumerate() {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325 ^ (chunk as u64);
        for b in name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        slot.copy_from_slice(&hash.to_be_bytes());
    }
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_always_produces_the_same_id() {
        assert_eq!(stable_id_for("llama3:8b"), stable_id_for("llama3:8b"));
    }

    #[test]
    fn different_names_produce_different_ids() {
        assert_ne!(stable_id_for("llama3:8b"), stable_id_for("qwen2.5:7b"));
    }
}
