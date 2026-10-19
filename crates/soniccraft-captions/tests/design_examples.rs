//! The examples of docs/DESIGN.md §10.1 and §10.3 reproduced literally (values and key order).

use soniccraft_captions::{
    Caption, CaptionConfig, CueInput, Word, build_captions, to_transcript_json, to_words_json,
};

fn example_captions() -> Vec<Caption> {
    let cue = CueInput {
        cue_id: "cue_01".into(),
        track_id: "voice_primary".into(),
        speaker: Some("Narrator".into()),
        text: "Autonomous software development".into(),
        start_ms: 0.0,
        end_ms: 2480.0,
        words: vec![
            Word::new("Autonomous", 0.0, 610.0),
            Word::new("software", 610.0, 1020.0),
            Word::new("development", 1020.0, 2480.0),
        ],
    };
    build_captions(&[cue], &CaptionConfig::default())
}

#[test]
fn words_json_example_section_10_1() {
    let value = to_words_json(&example_captions());
    assert_eq!(value["version"], "1.0");
    assert_eq!(
        serde_json::to_string(&value["captions"][0]).expect("serializable"),
        r#"{"index":1,"cue_id":"cue_01","track_id":"voice_primary","speaker":"Narrator","start_ms":0.0,"end_ms":2480.0,"text":"Autonomous software development","lines":["Autonomous software development"]}"#
    );
    assert_eq!(
        serde_json::to_string(&value["words"][0]).expect("serializable"),
        r#"{"text":"Autonomous","start_ms":0.0,"end_ms":610.0,"cue_id":"cue_01","track_id":"voice_primary","speaker":"Narrator"}"#
    );
    let keys: Vec<&str> = value
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert_eq!(keys, ["version", "captions", "words"]);
}

#[test]
fn transcript_example_section_10_3() {
    let value = to_transcript_json(&example_captions());
    let first_two: Vec<String> = value
        .as_array()
        .map(|a| {
            a.iter()
                .take(2)
                .map(|w| serde_json::to_string(w).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        first_two,
        [
            r#"{"id":"w0","text":"Autonomous","start":0.0,"end":0.61}"#,
            r#"{"id":"w1","text":"software","start":0.61,"end":1.02}"#,
        ]
    );
}
