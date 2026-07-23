//! Token usage tracking. Mirrors `NonNullableUsage` + `updateUsage` /
//! `accumulateUsage` in `src/services/api/logging.ts`.
//!
//! `message_start` carries input/cache tokens; `message_delta` carries output
//! tokens. `update_from_part` takes the meaningful (positive) value from each
//! part without clobbering; `accumulate` sums per-turn usage into a running
//! total across the whole query.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Merge a streaming usage fragment, keeping the positive value for each
    /// field (mirrors the `!== null && > 0 ? part : current` logic). Fields the
    /// fragment omits are left untouched.
    pub fn update_from_part(&mut self, part: &UsagePart) {
        if let Some(v) = part.input_tokens {
            if v > 0 {
                self.input_tokens = v;
            }
        }
        if let Some(v) = part.output_tokens {
            if v > 0 {
                self.output_tokens = v;
            }
        }
        if let Some(v) = part.cache_creation_input_tokens {
            if v > 0 {
                self.cache_creation_input_tokens = v;
            }
        }
        if let Some(v) = part.cache_read_input_tokens {
            if v > 0 {
                self.cache_read_input_tokens = v;
            }
        }
    }

    /// Sum another usage into this one (running total across turns).
    pub fn accumulate(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}

/// A streaming usage fragment as it appears in `message_start` / `message_delta`.
/// All fields optional — only the relevant subset is present in each event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsagePart {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_keeps_positive_without_clobbering() {
        let mut u = Usage {
            input_tokens: 100,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        // message_delta typically carries output tokens only.
        u.update_from_part(&UsagePart {
            output_tokens: Some(42),
            ..Default::default()
        });
        assert_eq!(u.input_tokens, 100); // untouched
        assert_eq!(u.output_tokens, 42);
    }

    #[test]
    fn accumulate_sums() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        a.accumulate(&Usage {
            input_tokens: 3,
            output_tokens: 7,
            ..Default::default()
        });
        assert_eq!(a.input_tokens, 13);
        assert_eq!(a.output_tokens, 12);
    }
}
