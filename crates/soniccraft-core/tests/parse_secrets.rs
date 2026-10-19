//! `secret_in_timeline` (DESIGN.md §2.8): annotation keys, secret-shaped keys, keys inside provider strings
//! and URL credentials; secrets never appear in any diagnostic; `secret_spans` finds them in the source.

mod common;

use common::*;
use soniccraft_core::SourceFormat;
use soniccraft_core::parse::secret_spans;

fn leaks(c: &soniccraft_core::Compiled, secret: &str) -> Vec<String> {
    c.diagnostics
        .iter()
        .map(|d| format!("{d} {:?} {:?}", d.path, d.related))
        .filter(|text| text.contains(secret))
        .collect()
}

#[test]
fn annotation_keys_that_name_secrets() {
    let c = yaml(
        r#"version: "1.0"
x-api-key: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
x-token: ghp_abcdefghijklmnopqrstuvwxyz0123456789
x_secret: plaintextsecretvalue
x-password: hunter2hunter2
tracks:
  - id: v
    type: speech
    provider: mock
    x-auth-token: eyJhbGciOiJIUzI1NiJ9.e30.abc
    sequence:
      - text: hi
"#,
    );
    for path in [
        "x-api-key",
        "x-token",
        "x_secret",
        "x-password",
        "tracks[0].x-auth-token",
    ] {
        expect(&c, "secret_in_timeline", path, &["api_key_env"]);
    }
    for secret in [
        "9f86d0818",
        "ghp_abcdefgh",
        "plaintextsecret",
        "hunter2hunter2",
        "eyJhbGci",
    ] {
        assert!(
            leaks(&c, secret).is_empty(),
            "{secret} leaked: {:?}",
            leaks(&c, secret)
        );
    }
}

#[test]
fn secret_shaped_keys_are_redacted_from_paths() {
    let key = "sk_live_0123456789abcdefABCDEF";
    let c = yaml(&format!(
        "version: \"1.0\"\nproviders:\n  eleven:\n    type: elevenlabs\n    {key}: true\ntracks:\n  - id: v\n    type: speech\n    provider: mock\n    sequence:\n      - text: hi\n"
    ));
    let d = expect(
        &c,
        "secret_in_timeline",
        "providers.eleven.<redacted>",
        &["api_key_env"],
    );
    // Positioned at the key even though the path is redacted.
    assert_eq!(d.line, Some(5));
    assert_eq!(count(&c, "unknown_field"), 0, "{}", dump(&c));
    assert!(leaks(&c, key).is_empty(), "{:?}", leaks(&c, key));
}

#[test]
fn keys_inside_provider_strings() {
    let bearer = "sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789";
    let argv = "sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e";
    let c = yaml(&format!(
        r#"version: "1.0"
providers:
  openai:
    settings: {{extra_body: {{authorization: "Bearer {bearer}"}}}}
  q: {{type: openai, base_url: "https://example.com/v1?api_key=abcdef0123456789"}}
  c: {{type: command, command: ["my-tts", "--api-key={argv}", "{{output}}"]}}
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      - text: "my key is not a secret here"
"#
    ));
    expect(
        &c,
        "secret_in_timeline",
        "providers.openai.settings.extra_body.authorization",
        &[],
    );
    expect(&c, "secret_in_timeline", "providers.q.base_url", &[]);
    expect(
        &c,
        "secret_in_timeline",
        "providers.c.command[1]",
        &["environment variable"],
    );
    for secret in [bearer, argv, "abcdef0123456789"] {
        assert!(leaks(&c, secret).is_empty(), "{:?}", leaks(&c, secret));
    }
}

#[test]
fn passwords_in_url_userinfo() {
    for (url, password) in [
        (
            "http://soniccraft:LEAKCHECKpassw0rd@127.0.0.1:9",
            "LEAKCHECKpassw0rd",
        ),
        (
            "http://apikey:sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e@127.0.0.1:9/v1",
            "sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e",
        ),
    ] {
        let c = yaml(&format!(
            "version: \"1.0\"\nproviders:\n  k: {{type: kokoro, base_url: \"{url}\"}}\ntracks:\n  - id: v\n    type: speech\n    provider: k\n    sequence:\n      - text: hi\n"
        ));
        let d = expect(
            &c,
            "secret_in_timeline",
            "providers.k.base_url",
            &["Remove the credentials from `base_url`"],
        );
        assert!(
            d.message.contains("password in the URL") || d.message.contains("API key"),
            "{}",
            d.message
        );
        assert!(leaks(&c, password).is_empty(), "{:?}", leaks(&c, password));
    }
    // A user name alone is not a secret.
    let c = yaml(
        "version: \"1.0\"\nproviders:\n  k: {type: kokoro, base_url: \"http://user@127.0.0.1:9\"}\ntracks:\n  - id: v\n    type: speech\n    provider: k\n    sequence:\n      - text: hi\n",
    );
    assert_eq!(count(&c, "secret_in_timeline"), 0, "{}", dump(&c));
}

// ---- secret_spans -----------------------------------------------------------------------------------------

fn masked(text: &str, format: SourceFormat) -> String {
    let mut out = String::new();
    let mut last = 0;
    let spans = secret_spans(text, format);
    for r in &spans {
        assert!(
            r.start >= last && r.start < r.end,
            "sorted, non-overlapping: {spans:?}"
        );
        out.push_str(&text[last..r.start]);
        out.push_str("***");
        last = r.end;
    }
    out.push_str(&text[last..]);
    out
}

#[test]
fn secret_spans_cover_every_reported_secret_in_yaml() {
    let text = r#"version: "1.0"
x-api-key: abcdef0123456789abcdef0123456789
providers:
  eleven:
    type: elevenlabs
    api_key: "plain-secret-value"
    sk_live_0123456789abcdefABCDEF: true
    settings: {auth: {user: bob, pass: hunter2hunter2}}
  k: {type: kokoro, base_url: "http://u:pw0123456789@127.0.0.1:9"}
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      - text: "say sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789 aloud"
"#;
    let out = masked(text, SourceFormat::Yaml);
    assert_eq!(
        out,
        r#"version: "1.0"
x-api-key: ***
providers:
  eleven:
    type: elevenlabs
    api_key: ***
    ***: true
    settings: {auth: {***: ***, ***: ***}}
  k: {type: kokoro, base_url: ***}
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      - text: "say *** aloud"
"#
    );
}

#[test]
fn secret_spans_in_json_and_unparsable_documents() {
    let text = "{\"version\": \"1.0\", \"providers\": {\"openai\": {\"api_key\": \"s3cr\\u0065t-value-here\"}}, \"tracks\": []}";
    assert_eq!(
        masked(text, SourceFormat::Json),
        "{\"version\": \"1.0\", \"providers\": {\"openai\": {\"api_key\": ***}}, \"tracks\": []}"
    );
    // Auto detects JSON the way parsing does.
    assert_eq!(
        masked(text, SourceFormat::Auto),
        masked(text, SourceFormat::Json)
    );
    // A document that does not parse still has its key-shaped tokens masked.
    let broken =
        "version: \"1.0\"\nproviders: {openai: {api_key: sk-proj-AbCdEfGhIjKlMnOpQrStUvWx\n";
    assert_eq!(
        masked(broken, SourceFormat::Yaml),
        "version: \"1.0\"\nproviders: {openai: {api_key: ***\n"
    );
    assert!(secret_spans("version: \"1.0\"\ntracks: []\n", SourceFormat::Yaml).is_empty());
}
