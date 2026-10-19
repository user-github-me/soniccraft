//! Typed access to free-form provider `settings` objects, plus the merged effective request parameters
//! ([`Resolved`]) every provider, the cache key and the estimator agree on.

use serde_json::{Map, Number, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};

use crate::defaults;

/// Effective request parameters: IR values with provider-specific fallbacks applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Resolved {
    pub provider_type: ProviderType,
    /// `voice.model`, else the instance `model`, else the provider default.
    pub model: Option<String>,
    /// `voice.voice_id`, else the provider default voice.
    pub voice_id: Option<String>,
    pub language: Option<String>,
    /// Instance `settings` overlaid with the voice `settings`, one level deep. Lowering already merges them,
    /// so this is idempotent for fully lowered programs; it protects hand-built requests.
    pub settings: Map<String, Value>,
}

impl Resolved {
    pub(crate) fn new(provider: &ProviderInstance, voice: &VoiceIr) -> Self {
        let t = provider.provider_type;
        let model = non_empty(voice.model.as_deref())
            .or_else(|| non_empty(provider.model.as_deref()))
            .or_else(|| defaults::default_model(t))
            .map(str::to_owned);
        let voice_id = non_empty(voice.voice_id.as_deref())
            .or_else(|| defaults::default_voice(t))
            .map(str::to_owned);
        let language = non_empty(voice.language.as_deref()).map(str::to_owned);
        Resolved {
            provider_type: t,
            model,
            voice_id,
            language,
            settings: merge(&provider.settings, &voice.settings),
        }
    }

    /// The model, or "" when there is none.
    pub(crate) fn model_str(&self) -> &str {
        self.model.as_deref().unwrap_or("")
    }
}

/// Trimmed, non-empty.
pub(crate) fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// `base` overlaid with `overlay`, key by key (later layer wins).
pub(crate) fn merge(base: &Map<String, Value>, overlay: &Map<String, Value>) -> Map<String, Value> {
    let mut out = base.clone();
    for (k, v) in overlay {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// A finite number, or a string holding one (YAML authors sometimes quote numbers).
pub(crate) fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

/// `settings[key]` as a finite number.
pub(crate) fn get_f64(settings: &Map<String, Value>, key: &str) -> Option<f64> {
    settings.get(key).and_then(value_f64)
}

/// `settings[key]` as a finite, strictly positive number.
pub(crate) fn get_positive(settings: &Map<String, Value>, key: &str) -> Option<f64> {
    get_f64(settings, key).filter(|x| *x > 0.0)
}

/// `settings[key]` as an integer (integral floats and numeric strings accepted).
pub(crate) fn get_i64(settings: &Map<String, Value>, key: &str) -> Option<i64> {
    match settings.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15)
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `settings[key]` as a boolean (`true`/`false` strings accepted).
pub(crate) fn get_bool(settings: &Map<String, Value>, key: &str) -> Option<bool> {
    match settings.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" => Some(true),
            "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `settings[key]` as a trimmed, non-empty string.
pub(crate) fn get_str(settings: &Map<String, Value>, key: &str) -> Option<String> {
    match settings.get(key)? {
        Value::String(s) => non_empty(Some(s)).map(str::to_owned),
        _ => None,
    }
}

/// `settings[key]` as an object.
pub(crate) fn get_object<'a>(
    settings: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Map<String, Value>> {
    settings.get(key).and_then(Value::as_object)
}

/// A JSON number for `x` (`None` for non-finite values).
pub(crate) fn number(x: f64) -> Option<Value> {
    Number::from_f64(x).map(Value::Number)
}

/// The primary language subtag of a BCP-47-ish tag: `en-US`, `en_us`, `EN` → `en`. `None` unless it is 2–3
/// ASCII letters.
pub(crate) fn primary_language(tag: &str) -> Option<String> {
    let primary = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
    ((2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_lowercase()))
        .then_some(primary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn typed_getters_are_lenient_but_strict_about_type() {
        let s = map(
            json!({"a": 1.5, "b": "2.5", "c": "x", "d": 3, "e": 4.0, "f": true, "g": "no", "h": " hi ",
                           "i": {"k": 1}, "j": -1}),
        );
        assert_eq!(get_f64(&s, "a"), Some(1.5));
        assert_eq!(get_f64(&s, "b"), Some(2.5));
        assert_eq!(get_f64(&s, "c"), None);
        assert_eq!(get_i64(&s, "d"), Some(3));
        assert_eq!(get_i64(&s, "e"), Some(4));
        assert_eq!(get_i64(&s, "a"), None);
        assert_eq!(get_bool(&s, "f"), Some(true));
        assert_eq!(get_bool(&s, "g"), Some(false));
        assert_eq!(get_str(&s, "h").as_deref(), Some("hi"));
        assert_eq!(get_str(&s, "a"), None);
        assert!(get_object(&s, "i").is_some());
        assert_eq!(get_positive(&s, "j"), None);
        assert_eq!(get_f64(&s, "missing"), None);
    }

    #[test]
    fn merge_overlays_one_level() {
        let base = map(json!({"a": 1, "b": {"x": 1}}));
        let over = map(json!({"b": {"y": 2}, "c": 3}));
        assert_eq!(
            Value::Object(merge(&base, &over)),
            json!({"a": 1, "b": {"y": 2}, "c": 3})
        );
    }

    #[test]
    fn primary_language_subtags() {
        assert_eq!(primary_language("en-US").as_deref(), Some("en"));
        assert_eq!(primary_language("pt_BR").as_deref(), Some("pt"));
        assert_eq!(primary_language("FIL").as_deref(), Some("fil"));
        assert_eq!(primary_language("english"), None);
        assert_eq!(primary_language(""), None);
    }
}
