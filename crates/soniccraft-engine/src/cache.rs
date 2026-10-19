//! TTS cache directory resolution and maintenance (DESIGN.md §5.1).

use std::path::{Path, PathBuf};

use crate::context::absolute;
use crate::{CacheInfo, Engine, TimelineSource};

/// The cache directory for a timeline whose base directory is `timeline_dir` (None: no timeline):
/// [`crate::EngineOptions::cache_dir`] > `SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache` >
/// `<current dir>/.soniccraft/cache`. Relative settings resolve against the current directory.
pub(crate) fn resolve(engine: &Engine, timeline_dir: Option<&Path>) -> PathBuf {
    let explicit = engine.options.cache_dir.as_deref().map(absolute);
    let fallback = match timeline_dir {
        Some(dir) => absolute(dir),
        None => absolute(Path::new(".")),
    };
    absolute(&soniccraft_tts::resolve_cache_dir(
        explicit.as_deref(),
        &engine.env,
        &fallback,
    ))
}

/// [`crate::Engine::cache_dir`].
pub(crate) fn dir(engine: &Engine, source: Option<&TimelineSource>) -> PathBuf {
    resolve(engine, source.map(|s| s.base_dir.as_path()))
}

/// [`crate::Engine::cache_info`].
pub(crate) fn info(engine: &Engine, source: Option<&TimelineSource>) -> std::io::Result<CacheInfo> {
    let dir = dir(engine, source);
    let stats = soniccraft_tts::cache_stats(&dir)?;
    Ok(CacheInfo {
        path: dir.display().to_string(),
        entries: stats.entries,
        bytes: stats.bytes,
    })
}

/// [`crate::Engine::cache_clear`].
pub(crate) fn clear(engine: &Engine, source: Option<&TimelineSource>) -> std::io::Result<usize> {
    soniccraft_tts::clear_cache(&dir(engine, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineOptions, EnvSource};
    use soniccraft_core::SourceFormat;

    #[test]
    fn resolution_order() {
        let source = TimelineSource::from_text("", "/project/episode", SourceFormat::Yaml);
        let plain = Engine::new(EngineOptions::default()).with_env(EnvSource::fixed([("A", "b")]));
        assert_eq!(
            plain.cache_dir(Some(&source)),
            PathBuf::from("/project/episode/.soniccraft/cache")
        );
        assert!(plain.cache_dir(None).ends_with(".soniccraft/cache"));
        assert!(plain.cache_dir(None).is_absolute());

        let env = plain
            .clone()
            .with_env(EnvSource::fixed([("SONICCRAFT_CACHE_DIR", "/env/cache")]));
        assert_eq!(env.cache_dir(Some(&source)), PathBuf::from("/env/cache"));

        let explicit = Engine::new(EngineOptions {
            cache_dir: Some("/explicit".into()),
            ..EngineOptions::default()
        })
        .with_env(EnvSource::fixed([("SONICCRAFT_CACHE_DIR", "/env/cache")]));
        assert_eq!(
            explicit.cache_dir(Some(&source)),
            PathBuf::from("/explicit")
        );
    }

    #[test]
    fn info_and_clear_on_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::new(EngineOptions {
            cache_dir: Some(tmp.path().join("none")),
            ..EngineOptions::default()
        });
        let info = engine.cache_info(None).unwrap();
        assert_eq!((info.entries, info.bytes), (0, 0));
        assert_eq!(engine.cache_clear(None).unwrap(), 0);
    }
}
