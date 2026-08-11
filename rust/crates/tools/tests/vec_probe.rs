//! Empirical probe for the memory vector store's noise floor.
//!
//! Documents why `VECTOR_NOISE_FLOOR = 0.1` was chosen: with sign hashing into
//! 256 dims, unrelated texts land around 1/sqrt(256) ≈ 0.06, while genuine
//! trigram overlap clears 0.1 comfortably. If these measurements drift, the
//! floor constant in `memory.rs` needs revisiting.

use nonoclaw_tools::memory::{VECTOR_NOISE_FLOOR, cosine_similarity, embed};

fn cosine(a: &str, b: &str) -> f64 {
    cosine_similarity(&embed(a), &embed(b))
}

#[test]
fn unrelated_texts_stay_below_floor() {
    let unrelated = cosine("pip", "rust-edition use 2024 edition Use Rust edition 2024 for new projects. rust");
    assert!(
        unrelated < VECTOR_NOISE_FLOOR,
        "unrelated cosine {unrelated} must stay below floor {VECTOR_NOISE_FLOOR}"
    );
}

#[test]
fn matching_texts_clear_the_floor() {
    let matching = cosine(
        "pip",
        "pip-mirror pip use tsinghua Always use tsinghua mirror for pip installs. pip",
    );
    assert!(
        matching > VECTOR_NOISE_FLOOR,
        "matching cosine {matching} must clear floor {VECTOR_NOISE_FLOOR}"
    );
}

#[test]
fn weak_trigram_overlap_stays_below_floor() {
    // "or " appears in both "mirror" and "for new" — a weak, sub-threshold link.
    let weak = cosine(
        "mirror installs",
        "rust-edition use 2024 edition Use Rust edition 2024 for new projects. rust",
    );
    assert!(
        weak < VECTOR_NOISE_FLOOR,
        "weak-overlap cosine {weak} must stay below floor {VECTOR_NOISE_FLOOR}"
    );
}
