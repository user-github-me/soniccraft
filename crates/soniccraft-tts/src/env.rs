//! Environment lookup (API keys, `KOKORO_BASE_URL`, `PIPER_VOICES_DIR`, `PATH`, `HOME`), injectable so tests and
//! embedders never depend on — or mutate — the process environment.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Where the TTS client reads environment variables from.
///
/// The `Debug` output lists variable *names* only, never values: values are usually API keys.
#[derive(Clone, Default)]
pub enum EnvSource {
    /// The process environment (the default).
    #[default]
    Process,
    /// A fixed set of variables. The process environment is ignored entirely (including `PATH` and `HOME`).
    Fixed(Arc<BTreeMap<String, String>>),
}

impl EnvSource {
    /// A fixed environment made of `vars`.
    pub fn fixed<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        EnvSource::Fixed(Arc::new(
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        ))
    }

    /// The value of `name` with surrounding whitespace removed, or `None` when it is unset, empty or not valid
    /// Unicode.
    pub fn get(&self, name: &str) -> Option<String> {
        let value = match self {
            EnvSource::Process => std::env::var(name).ok(),
            EnvSource::Fixed(vars) => vars.get(name).cloned(),
        };
        value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
    }

    /// The user's home directory: `HOME` from this source, falling back to the OS lookup for
    /// [`EnvSource::Process`].
    pub fn home_dir(&self) -> Option<std::path::PathBuf> {
        match self.get("HOME") {
            Some(home) => Some(home.into()),
            None if matches!(self, EnvSource::Process) => dirs::home_dir(),
            None => None,
        }
    }
}

impl fmt::Debug for EnvSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvSource::Process => f.write_str("EnvSource::Process"),
            EnvSource::Fixed(vars) => f
                .debug_tuple("EnvSource::Fixed")
                .field(&vars.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_env_trims_and_hides_values() {
        let env = EnvSource::fixed([("KEY", "  secret-value \n"), ("EMPTY", "   ")]);
        assert_eq!(env.get("KEY").as_deref(), Some("secret-value"));
        assert_eq!(env.get("EMPTY"), None);
        assert_eq!(env.get("MISSING"), None);
        let debug = format!("{env:?}");
        assert!(debug.contains("KEY"));
        assert!(
            !debug.contains("secret-value"),
            "Debug must not print values: {debug}"
        );
    }

    #[test]
    fn fixed_env_has_no_implicit_home() {
        assert_eq!(EnvSource::fixed([("A", "b")]).home_dir(), None);
        assert_eq!(
            EnvSource::fixed([("HOME", "/home/x")]).home_dir(),
            Some("/home/x".into())
        );
    }
}
