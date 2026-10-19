//! The web preview (`<stem>.preview.html`, DESIGN.md §10.2): the vendored single-file visualizer with the
//! render's payload embedded in its `<script id="soniccraft-data" type="application/json">` element.

use serde::Serialize;
use soniccraft_captions::{CaptionRecord, WordRecord};

/// The visualizer template, vendored from `packages/soniccraft-web-preview/preview.html` (a test keeps the
/// two byte-identical).
pub(crate) const TEMPLATE: &str = include_str!("../assets/preview.html");

/// The placeholder the payload replaces (exactly once).
pub(crate) const TOKEN: &str = "__SONICCRAFT_DATA__";

/// Version of the payload format.
pub(crate) const PAYLOAD_VERSION: u32 = 1;

/// Points per second of the waveform and the ducking envelopes.
pub(crate) const POINTS_PER_SECOND: u32 = 50;

/// The preview payload.
#[derive(Debug, Serialize)]
pub(crate) struct Payload<'a> {
    /// Always [`PAYLOAD_VERSION`].
    pub soniccraft_preview: u32,
    /// Timeline title, else the output file stem.
    pub title: &'a str,
    /// The audio file, relative to the HTML file.
    pub audio: &'a str,
    /// The render report as the manifest carries it: paths relative to the HTML file, no timings and no
    /// run provenance.
    pub report: &'a serde_json::Value,
    /// Min/max of the mastered mono mixdown per bucket.
    pub waveform: Waveform,
    /// Ducking gain curves.
    pub envelopes: Vec<Envelope>,
    /// Captions (words.json shape).
    pub captions: Vec<CaptionRecord>,
    /// Timed words (words.json shape).
    pub words: Vec<WordRecord>,
}

/// `waveform` of the payload.
#[derive(Debug, Serialize)]
pub(crate) struct Waveform {
    /// Buckets per second; bucket `i` covers `[i/pps, (i+1)/pps)`.
    pub points_per_second: u32,
    /// Minimum of each bucket.
    pub min: Vec<f64>,
    /// Maximum of each bucket.
    pub max: Vec<f64>,
}

/// One entry of `envelopes`.
#[derive(Debug, Serialize)]
pub(crate) struct Envelope {
    /// The ducked track.
    pub track_id: String,
    /// Samples per second; sample `i` is the gain at `t = i/pps`.
    pub points_per_second: u32,
    /// Gain in dB (0 = no attenuation).
    pub gain_db: Vec<f64>,
}

/// The preview page for `payload`.
pub(crate) fn render(payload: &Payload<'_>) -> Result<String, String> {
    let json = serde_json::to_string(payload)
        .map_err(|e| format!("cannot serialize the preview payload: {e}"))?;
    embed(TEMPLATE, &json)
}

/// Substitute `json` (made safe for a `<script>` element) for the single placeholder of `template`.
pub(crate) fn embed(template: &str, json: &str) -> Result<String, String> {
    let count = template.matches(TOKEN).count();
    if count != 1 {
        return Err(format!(
            "the preview template must contain the placeholder {TOKEN} exactly once (found {count})"
        ));
    }
    Ok(template.replacen(TOKEN, &escape_for_script(json), 1))
}

/// Make serialized JSON safe inside `<script type="application/json">`: every `<` becomes `<` (so
/// neither `</script` nor `<!--` can appear; escaping only `</` leaves the `<!-- … <script` hole open) and
/// U+2028/U+2029 become ` `/` `. The result is still the same JSON value: those characters can
/// only occur inside strings.
pub(crate) fn escape_for_script(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + json.len() / 32);
    for ch in json.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_has_one_placeholder_inside_the_data_script() {
        assert_eq!(TEMPLATE.matches(TOKEN).count(), 1);
        assert!(TEMPLATE.contains(&format!(
            "<script id=\"soniccraft-data\" type=\"application/json\">{TOKEN}</script>"
        )));
    }

    #[test]
    fn vendored_template_matches_the_package() {
        let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/soniccraft-web-preview/preview.html");
        let Ok(original) = std::fs::read(&package) else {
            return; // packaged crate: the web package is not available
        };
        assert!(
            original == TEMPLATE.as_bytes(),
            "crates/soniccraft-engine/assets/preview.html is out of date; resync it with:\n  cp \
             packages/soniccraft-web-preview/preview.html crates/soniccraft-engine/assets/preview.html"
        );
    }

    #[test]
    fn escaping_neutralizes_every_html_hazard() {
        let hostile = serde_json::json!({
            "a": "</script><script>alert(1)</script>",
            "b": "<!-- <script> -->",
            "c": "line\u{2028}sep\u{2029}",
            "d": "x < y && y > z"
        });
        let json = serde_json::to_string(&hostile).unwrap();
        let safe = escape_for_script(&json);
        assert!(!safe.contains('<'));
        assert!(!safe.contains('\u{2028}') && !safe.contains('\u{2029}'));
        let back: serde_json::Value = serde_json::from_str(&safe).unwrap();
        assert_eq!(back, hostile, "escaping keeps the JSON value");
    }

    #[test]
    fn embedding_replaces_exactly_once_and_ignores_dollar_patterns() {
        let page = embed(
            "<p>__SONICCRAFT_DATA__</p>",
            "{\"x\":\"$& $1 __SONICCRAFT_DATA__\"}",
        )
        .unwrap();
        assert_eq!(page, "<p>{\"x\":\"$& $1 __SONICCRAFT_DATA__\"}</p>");
        assert!(embed("none", "{}").is_err());
        assert!(embed("__SONICCRAFT_DATA__ __SONICCRAFT_DATA__", "{}").is_err());
    }
}
