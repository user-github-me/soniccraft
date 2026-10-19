//! Diagnostic helpers shared by validation and rendering: grouping of per-cue notes, did-you-mean hints for
//! missing asset files, and number formatting for messages.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path};

use soniccraft_core::diagnostics::codes;
use soniccraft_core::{Diagnostic, SpanIndex};

/// Most cue ids listed in one grouped diagnostic's message.
const MAX_LISTED: usize = 6;

/// One cue a grouped diagnostic is about.
#[derive(Debug, Clone)]
pub(crate) struct CueRef {
    /// Cue id.
    pub id: String,
    /// Cue path (`tracks[1].sequence[0]`).
    pub path: String,
    /// The cue's speaker key, if any.
    pub speaker: Option<String>,
}

/// Path for a diagnostic about `cues`: the cue itself when there is one, the shared speaker's entry when
/// every cue has the same speaker declared under `speakers:`, else `fallback` (or the first cue).
pub(crate) fn group_path(cues: &[CueRef], spans: &SpanIndex, fallback: Option<&str>) -> String {
    if let [only] = cues {
        return only.path.clone();
    }
    if let Some(first) = cues.first().and_then(|c| c.speaker.as_deref())
        && cues.iter().all(|c| c.speaker.as_deref() == Some(first))
    {
        let path = format!("speakers.{first}");
        if spans.get(&path).is_some() {
            return path;
        }
    }
    fallback
        .map(str::to_string)
        .or_else(|| cues.first().map(|c| c.path.clone()))
        .unwrap_or_default()
}

/// `` `a`, `b`, `c` `` (and "… (N more)").
pub(crate) fn list_ids(cues: &[CueRef]) -> String {
    soniccraft_core::diagnostics::list_names(cues.iter().map(|c| c.id.as_str()), MAX_LISTED)
}

/// Provider notes (`setting_ignored`, DESIGN.md §7) collected per cue, reported once per provider instance
/// and message.
#[derive(Debug, Default)]
pub(crate) struct Notes {
    seen: BTreeSet<(String, String)>,
    groups: Vec<(String, String, Vec<CueRef>)>,
    emitted: BTreeSet<(String, String)>,
}

impl Notes {
    /// Record that `provider` reported `message` for `cue`.
    pub(crate) fn add(&mut self, provider: &str, message: &str, cue: CueRef) {
        let key = (provider.to_string(), message.to_string());
        if self.seen.insert(key.clone()) {
            self.groups.push((key.0, key.1, vec![cue]));
        } else if let Some(group) = self
            .groups
            .iter_mut()
            .find(|g| g.0 == provider && g.1 == message)
            && !group.2.iter().any(|c| c.id == cue.id)
        {
            group.2.push(cue);
        }
    }

    /// `setting_ignored` diagnostics for the groups not returned by an earlier call.
    pub(crate) fn drain_new(&mut self, spans: &SpanIndex) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for (provider, message, cues) in &self.groups {
            let key = (provider.clone(), message.clone());
            if !self.emitted.insert(key) {
                continue;
            }
            let scope = if cues.len() == 1 {
                format!("cue `{}`", cues[0].id)
            } else {
                format!("{} cues ({})", cues.len(), list_ids(cues))
            };
            out.push(
                Diagnostic::info(
                    codes::SETTING_IGNORED,
                    format!("provider `{provider}` ({scope}): {message}"),
                )
                .with_path(group_path(cues, spans, None))
                .with_hint(
                    "Nothing needs to change; remove the setting (or change the model or output format) to \
                     silence this note",
                )
                .with_related(cues.iter().map(|c| c.id.clone())),
            );
        }
        out
    }
}

/// Audio file extensions listed in missing-asset hints.
const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "mp4", "aif", "aiff",
];

/// Most directory entries read for a did-you-mean hint.
const MAX_DIR_ENTRIES: usize = 10_000;

/// Hint for an `asset_not_found` diagnostic: a corrected path (as it should be written in the timeline)
/// when a file or directory with a close name exists, else what the directory contains.
///
/// `written` is the `source` value as written, `resolved` its absolute path, `base_dir` the timeline
/// directory relative paths resolve against.
pub(crate) fn missing_asset_hint(written: &str, resolved: &Path, base_dir: &Path) -> String {
    let relative_note = format!(
        "`source` paths are relative to the timeline directory `{}`",
        base_dir.display()
    );
    let Some(parent) = resolved.parent() else {
        return relative_note;
    };
    let (written_dir, _) = split_written(written);
    if !parent.is_dir() {
        return missing_directory_hint(written, resolved, base_dir).unwrap_or_else(|| {
            format!("The directory of `{written}` does not exist; {relative_note}")
        });
    }
    let wanted = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let names = entries(parent, false);
    if let Some(best) = best_match(&wanted, &names, true) {
        return format!("Did you mean `{written_dir}{best}`?");
    }
    let audio: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| is_audio(n))
        .collect();
    let shown = if written_dir.is_empty() {
        "the timeline directory".to_string()
    } else {
        format!("`{written_dir}`")
    };
    if audio.is_empty() {
        format!("{shown} contains no audio files; {relative_note}")
    } else {
        format!(
            "Audio files in {shown}: {}",
            soniccraft_core::diagnostics::list_names(audio.iter().copied(), 8)
        )
    }
}

/// When a directory component of `written` does not exist but a sibling with a close name does: the path
/// with that component corrected.
fn missing_directory_hint(written: &str, resolved: &Path, base_dir: &Path) -> Option<String> {
    let written_path = Path::new(written);
    if written_path.is_absolute()
        || written_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    // The deepest existing ancestor and the first missing component below it.
    let mut existing = resolved.parent()?;
    let mut missing: Option<OsString> = None;
    while !existing.is_dir() {
        missing = existing.file_name().map(|n| n.to_os_string());
        existing = existing.parent()?;
    }
    let missing = missing?.to_string_lossy().into_owned();
    let depth = existing.components().count();
    let base_depth = base_dir.components().count();
    let index = depth.checked_sub(base_depth)?;
    let mut parts: Vec<String> = written_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.get(index) != Some(&missing) {
        return None;
    }
    let directories = entries(existing, true);
    let best = best_match(&missing, &directories, false)?;
    parts[index] = best.to_string();
    Some(format!(
        "The directory `{missing}` does not exist; did you mean `{}`?",
        parts.join("/")
    ))
}

/// `("assets/sfx/", "whoosh.wav")` for `assets/sfx/whoosh.wav`.
fn split_written(written: &str) -> (&str, &str) {
    match written.rfind(['/', '\\']) {
        Some(i) => (&written[..=i], &written[i + 1..]),
        None => ("", written),
    }
}

fn is_audio(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Sorted names of the visible files (or directories) in `dir`.
fn entries(dir: &Path, directories: bool) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .take(MAX_DIR_ENTRIES)
        .filter(|e| {
            let path = e.path();
            if directories {
                path.is_dir()
            } else {
                path.is_file()
            }
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The candidate closest to `wanted`: the same name in another case, the same stem with another (audio)
/// extension, or a Jaro-Winkler match of at least 0.85.
fn best_match<'a>(wanted: &str, candidates: &'a [String], files: bool) -> Option<&'a str> {
    let lower = wanted.to_lowercase();
    if let Some(exact) = candidates.iter().find(|c| c.to_lowercase() == lower) {
        return Some(exact);
    }
    let stem = |s: &str| -> String {
        s.rsplit_once('.')
            .map_or(s, |(stem, _)| stem)
            .to_lowercase()
    };
    if files {
        let wanted_stem = stem(wanted);
        let mut same_stem = candidates.iter().filter(|c| stem(c) == wanted_stem);
        let first = same_stem.next();
        let audio = candidates
            .iter()
            .find(|c| stem(c) == wanted_stem && is_audio(c));
        if let Some(found) = audio.or(first) {
            return Some(found);
        }
    }
    candidates
        .iter()
        .map(|c| (c, strsim::jaro_winkler(&lower, &c.to_lowercase())))
        .filter(|(_, score)| *score >= 0.85)
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(c, _)| c.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_suggestions() {
        let tmp = tempfile::tempdir().unwrap();
        let sfx = tmp.path().join("assets/sfx");
        std::fs::create_dir_all(&sfx).unwrap();
        for name in ["whoosh.wav", "impact.wav", "notes.txt"] {
            std::fs::write(sfx.join(name), b"x").unwrap();
        }
        let hint =
            |written: &str| missing_asset_hint(written, &tmp.path().join(written), tmp.path());
        assert_eq!(
            hint("assets/sfx/whosh.wav"),
            "Did you mean `assets/sfx/whoosh.wav`?"
        );
        assert_eq!(
            hint("assets/sfx/whoosh.mp3"),
            "Did you mean `assets/sfx/whoosh.wav`?"
        );
        assert_eq!(
            hint("assets/sfx/Impact.WAV"),
            "Did you mean `assets/sfx/impact.wav`?"
        );
        let listing = hint("assets/sfx/zzz.wav");
        assert!(
            listing.contains("`impact.wav`") && listing.contains("`whoosh.wav`"),
            "{listing}"
        );
        assert!(!listing.contains("notes.txt"), "{listing}");
        assert_eq!(
            hint("asset/sfx/whoosh.wav"),
            "The directory `asset` does not exist; did you mean `assets/sfx/whoosh.wav`?"
        );
        assert_eq!(
            hint("assets/sfxx/whoosh.wav"),
            "The directory `sfxx` does not exist; did you mean `assets/sfx/whoosh.wav`?"
        );
        assert!(hint("nowhere/x.wav").contains("does not exist"));
    }

    #[test]
    fn notes_group_per_provider_and_message() {
        let spans = SpanIndex::new();
        let cue = |id: &str| CueRef {
            id: id.into(),
            path: format!("tracks[0].sequence[{}]", id.len()),
            speaker: None,
        };
        let mut notes = Notes::default();
        notes.add("el", "ignoring `style`", cue("a"));
        notes.add("el", "ignoring `style`", cue("bb"));
        notes.add("el", "ignoring `style`", cue("bb"));
        notes.add("oa", "ignoring `style`", cue("a"));
        let first = notes.drain_new(&spans);
        assert_eq!(first.len(), 2);
        assert!(first[0].message.contains("2 cues"), "{}", first[0].message);
        assert_eq!(first[0].related, ["a", "bb"]);
        assert_eq!(first[1].path.as_deref(), Some("tracks[0].sequence[1]"));
        notes.add("el", "ignoring `style`", cue("ccc"));
        notes.add("el", "fell back", cue("a"));
        let second = notes.drain_new(&spans);
        assert_eq!(second.len(), 1);
        assert!(second[0].message.ends_with("fell back"));
    }
}
