//! `speakers_share_voice`: distinct speakers whose cues resolve to one identical voice (DESIGN.md §2.7).
//!
//! Two speakers sound the same when their cues use the same provider instance, `voice_id`, `model` and
//! merged `settings` (language and gain do not change the voice). Typical cause: speakers that set only a
//! provider and fall back to its default voice (`mock`, `alloy`, `af_heart`, `Samantha`).

use indexmap::IndexMap;
use serde_json::Value;

use super::Lowerer;
use crate::diagnostics::{Diagnostic, codes};
use crate::ir::{NodeId, ProviderType, VoiceIr};

/// The first cue of one speaker that uses a shared voice.
struct SpeakerUse {
    node: NodeId,
}

/// A voice in a form that compares equal exactly when two cues sound the same: settings keys are sorted, so
/// `{a: 1, b: 2}` and `{b: 2, a: 1}` are one voice, and numbers compare by value (`speed: 1` and `speed: 1.0`
/// are one setting, as providers read them).
fn voice_key(voice: &VoiceIr) -> String {
    fn canonical(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), canonical(&map[k])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            Value::Number(n) => n
                .as_f64()
                .and_then(serde_json::Number::from_f64)
                .map_or_else(|| v.clone(), Value::Number),
            other => other.clone(),
        }
    }
    let settings = canonical(&Value::Object(voice.settings.clone()));
    serde_json::json!([voice.provider, voice.voice_id, voice.model, settings]).to_string()
}

/// `a`, `a` and `b`, `a`, `b` and `c` (backticked).
fn and_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

impl Lowerer<'_> {
    /// One warning per voice that the cues of two or more distinct speakers share.
    pub(super) fn speakers_share_voice(&mut self) {
        // voice key → speaker → its first cue with that voice, both in document order.
        let mut by_voice: IndexMap<String, IndexMap<&str, SpeakerUse>> = IndexMap::new();
        for (node, n) in self.nodes.iter().enumerate() {
            let Some(cue) = n.as_cue() else { continue };
            let Some(speaker) = cue.speaker.as_deref() else {
                continue;
            };
            // Placeholder voices (unresolvable provider or voice) and misspelled speakers were reported already.
            if cue.voice.provider.is_empty() || self.speaker_errors.contains(&node) {
                continue;
            }
            by_voice
                .entry(voice_key(&cue.voice))
                .or_default()
                .entry(speaker)
                .or_insert(SpeakerUse { node });
        }

        let mut found = Vec::new();
        for speakers in by_voice.values() {
            if speakers.len() < 2 {
                continue;
            }
            let names: Vec<&str> = speakers.keys().copied().collect();
            let (_, second) = speakers.get_index(1).expect("two speakers");
            let first_node = &self.nodes[speakers[0].node];
            let Some(cue) = first_node.as_cue() else {
                continue;
            };
            let voice = &cue.voice;
            let mut described = format!(
                "voice `{}` of provider `{}`",
                voice.voice_id.as_deref().unwrap_or("default"),
                voice.provider
            );
            if let Some(model) = &voice.model {
                described.push_str(&format!(", model `{model}`"));
            }
            if !voice.settings.is_empty() {
                described.push_str(", with the same settings");
            }
            let where_to = |name: &str, node: NodeId| {
                if self.local_voice_ids.contains(&node) {
                    format!("on the cues of `{name}`")
                } else {
                    format!("under `speakers.{name}`")
                }
            };
            let examples: Vec<String> = speakers
                .iter()
                .map(|(name, used)| {
                    let id = if voice.provider_type == ProviderType::Mock {
                        name.to_string()
                    } else {
                        "...".to_string()
                    };
                    format!("`voice_id: {id}` {}", where_to(name, used.node))
                })
                .collect();
            let hint = if voice.provider_type == ProviderType::Mock {
                format!(
                    "Give each speaker its own `voice_id`; for mock any string works and gives a distinct voice: {}",
                    examples.join(", ")
                )
            } else {
                format!(
                    "Give each speaker its own `voice_id`: {}; run `soniccraft voices {}` to list the voices",
                    examples.join(", "),
                    voice.provider_type
                )
            };
            let second_node = &self.nodes[second.node];
            let verb = if names.len() == 2 {
                "both use"
            } else {
                "all use"
            };
            found.push(
                Diagnostic::warning(
                    codes::SPEAKERS_SHARE_VOICE,
                    format!(
                        "speakers {} {verb} the {described}, so they sound identical",
                        and_list(&names)
                    ),
                )
                .with_path(second_node.path.clone())
                .with_hint(hint)
                .with_related(speakers.values().map(|u| self.nodes[u.node].id.clone())),
            );
        }
        self.diags.extend(found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_read_naturally() {
        assert_eq!(and_list(&["a"]), "`a`");
        assert_eq!(and_list(&["a", "b"]), "`a` and `b`");
        assert_eq!(and_list(&["a", "b", "c"]), "`a`, `b` and `c`");
    }

    #[test]
    fn settings_order_does_not_change_the_voice() {
        let voice = |settings: Value| VoiceIr {
            provider: "mock".into(),
            provider_type: ProviderType::Mock,
            voice_id: Some("v".into()),
            model: None,
            language: None,
            settings: settings.as_object().cloned().unwrap_or_default(),
        };
        let a = voice(serde_json::json!({"speed": 1.1, "x": {"b": 1, "a": 2}}));
        let b = voice(serde_json::json!({"x": {"a": 2, "b": 1}, "speed": 1.1}));
        assert_eq!(voice_key(&a), voice_key(&b));
        // Integer and float spellings of one number are one setting.
        assert_eq!(
            voice_key(&voice(serde_json::json!({"speed": 1}))),
            voice_key(&voice(serde_json::json!({"speed": 1.0})))
        );
        assert_ne!(
            voice_key(&a),
            voice_key(&voice(serde_json::json!({"speed": 1.2})))
        );
    }
}
