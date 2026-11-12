//! Where ScriptRunner keeps its data. Shared by the CLI, the daemon and the GUI so that all of
//! them resolve the same directory (and therefore the same IPC endpoint).

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable that overrides the data directory.
pub const DATA_DIR_ENV: &str = "SCRIPTRUNNER_DATA_DIR";

/// The data directory: `explicit` if given, else `SCRIPTRUNNER_DATA_DIR`, else the platform
/// default (`%ProgramData%\ScriptRunner` on Windows, `$XDG_DATA_HOME/scriptrunner` or
/// `~/.local/share/scriptrunner` elsewhere).
pub fn data_dir(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| resolve(cfg!(windows), |name| std::env::var_os(name)))
}

/// `data_dir` with the environment and the platform passed in, so it can be tested.
fn resolve(windows: bool, env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    // An empty variable counts as unset: `SCRIPTRUNNER_DATA_DIR=` must not mean "the current directory".
    let get = |name: &str| env(name).filter(|value| !value.is_empty()).map(PathBuf::from);

    if let Some(dir) = get(DATA_DIR_ENV) {
        return dir;
    }
    if windows {
        let base = get("PROGRAMDATA").unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        return base.join("ScriptRunner");
    }
    let base = get("XDG_DATA_HOME")
        .or_else(|| get("HOME").map(|home| home.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("scriptrunner")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn explicit_path_wins_over_everything() {
        assert_eq!(data_dir(Some(PathBuf::from("/x"))), PathBuf::from("/x"));
    }

    #[test]
    fn the_environment_variable_overrides_the_platform_default() {
        let vars = [(DATA_DIR_ENV, "/data"), ("HOME", "/home/u")];
        assert_eq!(resolve(false, env(&vars)), PathBuf::from("/data"));
        assert_eq!(resolve(true, env(&vars)), PathBuf::from("/data"));
    }

    #[test]
    fn an_empty_variable_is_ignored() {
        let vars = [(DATA_DIR_ENV, ""), ("HOME", "/home/u")];
        assert_eq!(
            resolve(false, env(&vars)),
            PathBuf::from("/home/u/.local/share/scriptrunner")
        );
    }

    #[test]
    fn windows_default_is_program_data() {
        let vars = [("PROGRAMDATA", "D:\\PD")];
        assert_eq!(resolve(true, env(&vars)), PathBuf::from("D:\\PD").join("ScriptRunner"));
        assert_eq!(
            resolve(true, env(&[])),
            PathBuf::from("C:\\ProgramData").join("ScriptRunner")
        );
    }

    #[test]
    fn unix_default_prefers_xdg_then_home() {
        let both = [("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")];
        assert_eq!(resolve(false, env(&both)), PathBuf::from("/xdg/scriptrunner"));
        let home = [("HOME", "/home/u")];
        assert_eq!(
            resolve(false, env(&home)),
            PathBuf::from("/home/u/.local/share/scriptrunner")
        );
        assert_eq!(resolve(false, env(&[])), PathBuf::from("./scriptrunner"));
    }
}
