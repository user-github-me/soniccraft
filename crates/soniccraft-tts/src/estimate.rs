//! Dry-run duration estimates (DESIGN.md §5.3).
//!
//! `seconds = spoken_chars / cps / speed`, where `spoken_chars` counts letters, digits, punctuation and single
//! spaces (`[audio tags]` excluded), `cps` is the instance `chars_per_second`, else the provider default
//! (elevenlabs 15.5, or 14 for `eleven_v3*`/`eleven_v4*`; openai 15; cartesia 15; kokoro 16; piper 17;
//! macos_say 17; command 15). With only `words_per_minute` configured: `seconds = words / (wpm / 60) / speed`.
//!
//! `speed` is the provider's speed setting: `settings.speed` (elevenlabs: also `voice_settings.speed`, ignored
//! for `eleven_v4*`; cartesia: also `generation_config.speed`), macos_say `settings.rate / 175` when `rate` is
//! set. Piper multiplies by its effective `length_scale` instead, and the command provider ignores speed (it
//! is never passed to the program). The mock provider's estimate is its exact output length.

use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};

use crate::defaults;
use crate::providers::{elevenlabs, macos_say, mock, piper};
use crate::settings::{Resolved, get_object, get_positive};
use crate::text;

/// Estimated length in seconds of `text` spoken by `voice` on `provider`, before silence trimming.
pub fn estimate_duration_secs(provider: &ProviderInstance, voice: &VoiceIr, text: &str) -> f64 {
    let resolved = Resolved::new(provider, voice);
    if resolved.provider_type == ProviderType::Mock {
        return mock::duration_secs(text, &resolved.settings);
    }
    let base = base_seconds(provider, &resolved, text);
    match resolved.provider_type {
        ProviderType::Piper => base * piper::length_scale(&resolved.settings),
        _ => base / speed_factor(&resolved),
    }
}

fn valid_rate(x: Option<f64>) -> Option<f64> {
    x.filter(|v| v.is_finite() && *v > 0.0)
}

fn base_seconds(provider: &ProviderInstance, resolved: &Resolved, text: &str) -> f64 {
    if let Some(cps) = valid_rate(provider.chars_per_second) {
        return text::spoken_chars(text) as f64 / cps;
    }
    if let Some(wpm) = valid_rate(provider.words_per_minute) {
        return text::spoken_word_count(text) as f64 / (wpm / 60.0);
    }
    text::spoken_chars(text) as f64
        / defaults::default_chars_per_second(resolved.provider_type, resolved.model.as_deref())
}

/// The speed multiplier the provider applies (1 = normal), clamped to 0.1–10.
pub(crate) fn speed_factor(resolved: &Resolved) -> f64 {
    let s = &resolved.settings;
    let nested = |object: &str| get_object(s, object).and_then(|o| get_positive(o, "speed"));
    let speed = match resolved.provider_type {
        ProviderType::Elevenlabs if elevenlabs::is_v4(resolved.model_str()) => None,
        ProviderType::Elevenlabs => get_positive(s, "speed").or_else(|| nested("voice_settings")),
        ProviderType::Cartesia => get_positive(s, "speed").or_else(|| nested("generation_config")),
        ProviderType::Openai | ProviderType::Kokoro => get_positive(s, "speed"),
        ProviderType::MacosSay => get_positive(s, "rate")
            .map(|rate| rate / macos_say::DEFAULT_RATE_WPM)
            .or_else(|| get_positive(s, "speed")),
        ProviderType::Piper | ProviderType::Command | ProviderType::Mock => None,
    };
    speed.unwrap_or(1.0).clamp(0.1, 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, voice};
    use serde_json::json;

    fn est(t: ProviderType, model: Option<&str>, settings: serde_json::Value, text: &str) -> f64 {
        estimate_duration_secs(
            &instance(t),
            &voice(t, Some("v"), model, None, settings),
            text,
        )
    }

    const TEXT: &str = "Autonomous software development is here."; // 40 spoken chars

    #[test]
    fn default_rates() {
        assert_eq!(text::spoken_chars(TEXT), 40);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(
            est(ProviderType::Elevenlabs, None, json!({}), TEXT),
            40.0 / 15.5
        ));
        assert!(close(
            est(ProviderType::Elevenlabs, Some("eleven_v3"), json!({}), TEXT),
            40.0 / 14.0
        ));
        assert!(close(
            est(ProviderType::Openai, None, json!({}), TEXT),
            40.0 / 15.0
        ));
        assert!(close(
            est(ProviderType::Cartesia, None, json!({}), TEXT),
            40.0 / 15.0
        ));
        assert!(close(
            est(ProviderType::Kokoro, None, json!({}), TEXT),
            40.0 / 16.0
        ));
        assert!(close(
            est(ProviderType::Piper, None, json!({}), TEXT),
            40.0 / 17.0
        ));
        assert!(close(
            est(ProviderType::MacosSay, None, json!({}), TEXT),
            40.0 / 17.0
        ));
        assert!(close(
            est(ProviderType::Command, None, json!({}), TEXT),
            40.0 / 15.0
        ));
        assert_eq!(est(ProviderType::Openai, None, json!({}), "[laughs]"), 0.0);
    }

    #[test]
    fn speed_settings() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(
            est(ProviderType::Openai, None, json!({"speed": 2.0}), TEXT),
            40.0 / 15.0 / 2.0
        ));
        assert!(close(
            est(
                ProviderType::Elevenlabs,
                None,
                json!({"voice_settings": {"speed": 1.25}}),
                TEXT
            ),
            40.0 / 15.5 / 1.25
        ));
        // eleven_v4 drops speed, so the estimate ignores it.
        assert!(close(
            est(
                ProviderType::Elevenlabs,
                Some("eleven_v4"),
                json!({"speed": 1.2}),
                TEXT
            ),
            40.0 / 14.0
        ));
        assert!(close(
            est(
                ProviderType::Cartesia,
                None,
                json!({"generation_config": {"speed": 1.5}}),
                TEXT
            ),
            40.0 / 15.0 / 1.5
        ));
        assert!(close(
            est(ProviderType::MacosSay, None, json!({"rate": 350}), TEXT),
            40.0 / 17.0 / 2.0
        ));
        assert!(close(
            est(
                ProviderType::Piper,
                None,
                json!({"length_scale": 1.5}),
                TEXT
            ),
            40.0 / 17.0 * 1.5
        ));
        assert!(close(
            est(ProviderType::Piper, None, json!({"speed": 2.0}), TEXT),
            40.0 / 17.0 * 0.5
        ));
        assert!(close(
            est(ProviderType::Command, None, json!({"speed": 2.0}), TEXT),
            40.0 / 15.0
        ));
        assert!(close(
            est(ProviderType::Openai, None, json!({"speed": "fast"}), TEXT),
            40.0 / 15.0
        ));
    }

    #[test]
    fn instance_rates() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let mut p = instance(ProviderType::Openai);
        let v = voice(ProviderType::Openai, None, None, None, json!({}));
        p.chars_per_second = Some(20.0);
        p.words_per_minute = Some(60.0);
        assert!(close(estimate_duration_secs(&p, &v, TEXT), 2.0));
        p.chars_per_second = None;
        assert!(close(estimate_duration_secs(&p, &v, TEXT), 5.0)); // 5 words at 1 word/s
        p.words_per_minute = Some(0.0);
        assert!(close(estimate_duration_secs(&p, &v, TEXT), 40.0 / 15.0));
    }

    #[test]
    fn mock_estimate_is_exact() {
        let p = instance(ProviderType::Mock);
        let v = voice(
            ProviderType::Mock,
            Some("narrator"),
            None,
            None,
            json!({"wpm": 180}),
        );
        let resolved = Resolved::new(&p, &v);
        let (audio, _) = mock::synthesize(TEXT, "narrator", &resolved.settings);
        assert_eq!(estimate_duration_secs(&p, &v, TEXT), audio.duration_secs());
    }
}
