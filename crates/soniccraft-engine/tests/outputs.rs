//! Output safety: the two-phase commit (a failed or cancelled render never touches earlier outputs), files
//! that are not SonicCraft outputs are never replaced, and document output paths can be confined to a
//! directory.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use soniccraft_engine::{
    Engine, EngineOptions, ProgressEvent, RenderOverrides, Severity, ValidateOptions,
};

/// Every file below `dir` (relative path → bytes), hidden temporary files included.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    out
}

fn everything(output: PathBuf, cancel: Option<Arc<AtomicBool>>) -> RenderOverrides {
    RenderOverrides {
        output_path: Some(output),
        export_subtitles: Some(true),
        export_stems: Some(true),
        export_manifest: Some(true),
        preview: true,
        cancel,
        ..RenderOverrides::default()
    }
}

/// An engine that sets `flag` when the render reaches `stage`.
fn cancelling_engine(project: &Project, stage: &str, flag: Arc<AtomicBool>) -> Engine {
    let target = stage.to_string();
    Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        progress: Some(Arc::new(move |event: &ProgressEvent| {
            if let ProgressEvent::Stage { stage, .. } = event
                && *stage == target
            {
                flag.store(true, Ordering::Relaxed);
            }
        })),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]))
}

/// A re-render of a *different* timeline to the same outputs, cancelled at every stage, leaves the
/// previous render's files byte for byte as they were, with no temporary files.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_re_renders_keep_the_previous_outputs() {
    let project = Project::new();
    let out = project.join("out/episode.wav");
    let first = project.timeline("first.yaml", README);
    project
        .engine()
        .render(&first, &everything(out.clone(), None))
        .await
        .unwrap();
    let before = snapshot(&project.join("out"));
    assert!(before.len() >= 10, "{:?}", before.keys());
    // Different text: a commit would change every file.
    let second = project.timeline(
        "second.yaml",
        &README.replace("Exactly.", "Absolutely right."),
    );
    for stage in [
        "parse",
        "decode",
        "synthesize",
        "process",
        "layout",
        "mix",
        "master",
        "encode",
        "export",
    ] {
        let flag = Arc::new(AtomicBool::new(false));
        let engine = cancelling_engine(&project, stage, Arc::clone(&flag));
        let failure = engine
            .render(&second, &everything(out.clone(), Some(flag)))
            .await
            .unwrap_err();
        assert!(failure.cancelled, "{stage}: {failure:?}");
        assert!(
            snapshot(&project.join("out")) == before,
            "{stage}: the previous outputs changed"
        );
    }
    // Not cancelled, the second render replaces them all.
    project
        .engine()
        .render(&second, &everything(out.clone(), None))
        .await
        .unwrap();
    let after = snapshot(&project.join("out"));
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    assert_ne!(
        after[Path::new("episode.wav")],
        before[Path::new("episode.wav")]
    );
    assert_ne!(
        after[Path::new("episode.vtt")],
        before[Path::new("episode.vtt")]
    );
}

/// A render that fails while writing a side file (here: a stems directory that cannot be written) keeps the
/// previous outputs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_side_file_keeps_the_previous_outputs() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    let out = project.join("out/episode.wav");
    let first = project.timeline("first.yaml", README);
    let engine = project.engine();
    engine
        .render(&first, &everything(out.clone(), None))
        .await
        .unwrap();
    let before = snapshot(&project.join("out"));
    let stems = project.join("out/episode.stems");
    std::fs::set_permissions(&stems, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(stems.join("probe"), b"x").is_ok() {
        eprintln!("skipping: permissions are not enforced here (root?)");
        return;
    }
    let second = project.timeline(
        "second.yaml",
        &README.replace("Exactly.", "Absolutely right."),
    );
    let failure = engine
        .render(&second, &everything(out.clone(), None))
        .await
        .unwrap_err();
    std::fs::set_permissions(&stems, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!failure.cancelled);
    let d = failure
        .diagnostics
        .iter()
        .find(|d| d.code == "encode_failed")
        .unwrap_or_else(|| panic!("{failure:#?}"));
    assert!(d.message.contains("stem of track"), "{}", d.message);
    assert!(
        snapshot(&project.join("out")) == before,
        "the previous outputs changed"
    );
}

/// The reviewers' repro: a directory where a subtitle file goes is refused before anything runs, and the
/// previous audio and subtitles survive.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_in_the_way_is_refused_up_front() {
    let project = Project::new();
    let out = project.join("out/ep.wav");
    let source = project.timeline("small.yaml", README);
    let engine = project.engine();
    let subtitles = RenderOverrides {
        output_path: Some(out.clone()),
        export_subtitles: Some(true),
        ..RenderOverrides::default()
    };
    engine.render(&source, &subtitles).await.unwrap();
    std::fs::remove_file(project.join("out/ep.srt")).unwrap();
    std::fs::create_dir(project.join("out/ep.srt")).unwrap();
    let before = snapshot(&project.join("out"));
    let cached = soniccraft_tts::cache_stats(&project.cache_dir())
        .unwrap()
        .entries;
    let changed = project.timeline("small.yaml", &README.replace("Exactly.", "Right."));
    let failure = engine.render(&changed, &subtitles).await.unwrap_err();
    let d = &failure.diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.severity),
        ("invalid_value", Severity::Error)
    );
    assert_eq!(
        d.message,
        "refusing to overwrite `ep.srt`: it is a directory, not a SonicCraft output"
    );
    assert!(
        d.hint
            .as_deref()
            .unwrap()
            .contains("delete or rename `ep.srt`")
    );
    assert!(snapshot(&project.join("out")) == before);
    assert_eq!(
        soniccraft_tts::cache_stats(&project.cache_dir())
            .unwrap()
            .entries,
        cached,
        "refused before synthesis"
    );
}

/// An existing file that is not an audio file is never replaced by the audio output.
#[tokio::test(flavor = "multi_thread")]
async fn files_that_are_not_outputs_are_never_overwritten() {
    let project = Project::new();
    let notes = project.join("notes.txt");
    std::fs::write(&notes, b"my notes").unwrap();
    let engine = project.engine();
    let source = project.timeline("briefing.yaml", README);
    // As a render option (the extension does not select a format: `format: wav` is in the document).
    let failure = engine
        .render(
            &source,
            &RenderOverrides {
                output_path: Some(notes.clone()),
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap_err();
    let d = failure
        .diagnostics
        .iter()
        .find(|d| d.code == "invalid_value")
        .unwrap_or_else(|| panic!("{failure:#?}"));
    assert_eq!(
        d.message,
        "refusing to overwrite `notes.txt`: it is not a SonicCraft output"
    );
    assert!(
        d.path.is_none(),
        "a render option has no document path: {d:?}"
    );
    assert!(d.hint.as_deref().unwrap().contains("audio extension"));
    // As written in the document.
    let text = README.replace("format: wav", "format: wav\n  path: notes.txt");
    let failure = engine
        .render(
            &project.timeline("doc.yaml", &text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap_err();
    let d = failure
        .diagnostics
        .iter()
        .find(|d| d.code == "invalid_value")
        .unwrap();
    assert_eq!(d.path.as_deref(), Some("output.path"));
    assert!(d.line.is_some());
    assert_eq!(std::fs::read(&notes).unwrap(), b"my notes");
    // A previous audio output is replaced as usual.
    let report = engine
        .render(
            &project.timeline("doc2.yaml", README),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    let again = engine
        .render(
            &project.timeline("doc2.yaml", README),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.output.path, again.output.path);
}

fn confined(project: &Project) -> Engine {
    Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        confine_outputs_to: Some(project.join("base")),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]))
}

const SHORT: &str = r#"version: "1.0"
output: { format: wav, path: OUT }
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Confined.", provider: mock }
"#;

/// With `confine_outputs_to`, an `output.path` from the document that leaves the directory (by `..` or a
/// symlink) is an error in validate and render, and nothing is written; render options are exempt.
#[tokio::test(flavor = "multi_thread")]
async fn document_output_paths_stay_inside_the_confinement() {
    let project = Project::new();
    std::fs::create_dir_all(project.join("base")).unwrap();
    std::fs::create_dir_all(project.join("outside")).unwrap();
    let engine = confined(&project);
    let mut escapes = vec!["../outside/escaped.wav".to_string()];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(project.join("outside"), project.join("base/link")).unwrap();
        escapes.push("link/escaped.wav".to_string());
    }
    for escape in &escapes {
        let text = SHORT.replace("OUT", escape);
        let source = project.timeline("base/t.yaml", &text);
        let report = engine.validate(&source, &ValidateOptions::default()).await;
        assert!(!report.valid, "{escape}");
        let d = report
            .diagnostics
            .iter()
            .find(|d| d.code == "invalid_value")
            .unwrap_or_else(|| panic!("{escape}: {report:#?}"));
        assert_eq!(d.path.as_deref(), Some("output.path"));
        assert!(d.message.contains("resolves outside"), "{}", d.message);
        assert!(d.hint.as_deref().unwrap().contains("output_path"));
        let failure = engine
            .render(&source, &RenderOverrides::default())
            .await
            .unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|d| d.code == "invalid_value" && d.path.as_deref() == Some("output.path")),
            "{failure:#?}"
        );
        assert!(
            std::fs::read_dir(project.join("outside"))
                .unwrap()
                .next()
                .is_none(),
            "nothing was written outside"
        );
    }
    // Inside: fine.
    let inside = project.timeline("base/ok.yaml", &SHORT.replace("OUT", "renders/ok.wav"));
    engine
        .render(&inside, &RenderOverrides::default())
        .await
        .unwrap();
    assert!(project.join("base/renders/ok.wav").exists());
    // An explicit output path is the caller's choice.
    let source = project.timeline("base/t.yaml", &SHORT.replace("OUT", "../outside/x.wav"));
    let report = engine
        .render(
            &source,
            &RenderOverrides {
                output_path: Some(project.join("outside/chosen.wav")),
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap();
    assert!(Path::new(&report.output.path).ends_with("outside/chosen.wav"));
}

/// A timeline that itself lies outside the confinement cannot fix a confined `output.path` with another
/// relative path (they all resolve next to it), so the hint points at `output_path` and `base_dir` instead
/// of suggesting `output.path: episode.wav` (MCP verifier t_basedir item 20).
#[tokio::test(flavor = "multi_thread")]
async fn a_timeline_outside_the_confinement_gets_a_hint_that_works() {
    let project = Project::new();
    std::fs::create_dir_all(project.join("base")).unwrap();
    std::fs::create_dir_all(project.join("elsewhere")).unwrap();
    let engine = confined(&project);
    let hint_of = |diagnostics: &[soniccraft_core::Diagnostic]| {
        diagnostics
            .iter()
            .find(|d| d.code == "invalid_value" && d.path.as_deref() == Some("output.path"))
            .and_then(|d| d.hint.clone())
            .unwrap_or_else(|| panic!("{diagnostics:#?}"))
    };
    let outside = project.timeline("elsewhere/t.yaml", &SHORT.replace("OUT", "episode.wav"));
    let report = engine.validate(&outside, &ValidateOptions::default()).await;
    let hint = hint_of(&report.diagnostics);
    assert!(hint.contains("timeline itself is outside"), "{hint}");
    assert!(
        hint.contains("output_path") && hint.contains("base_dir"),
        "{hint}"
    );
    assert!(!hint.contains("output.path: episode.wav"), "{hint}");
    let failure = engine
        .render(&outside, &RenderOverrides::default())
        .await
        .unwrap_err();
    assert_eq!(hint_of(&failure.diagnostics), hint);
    // A timeline inside the confinement keeps the relative-path advice.
    let inside = project.timeline("base/t.yaml", &SHORT.replace("OUT", "../outside/x.wav"));
    let report = engine.validate(&inside, &ValidateOptions::default()).await;
    assert!(hint_of(&report.diagnostics).contains("output.path: episode.wav"));
}
