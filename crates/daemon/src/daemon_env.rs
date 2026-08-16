//! The environment the daemon resolves credentials from (spec 0180).
//!
//! The daemon's own process environment is fixed at launch and cannot be
//! refreshed: `/construct restart` re-`exec()`s the running image, which
//! carries the same environment across, so a key exported after the daemon
//! started stays invisible until something stops and re-spawns the process
//! from a shell that has it. That made "export the key" the only way to
//! reach an API-key surface, and a surprising one — editing config.toml is
//! picked up by a restart, exporting a variable is not.
//!
//! `[daemon.env]` closes that: a table of `KEY = "value"` pairs that the
//! daemon layers *underneath* its real environment. Reads go through this
//! module rather than `std::env::var` directly, and the same pairs are
//! merged into the base environment of every session the daemon spawns, so
//! declaring a credential in config.toml behaves like exporting it in the
//! shell that launched the daemon.
//!
//! Precedence is one-directional: a variable that is really set (to a
//! non-empty value) always wins. Config fills gaps, it does not override an
//! user who exported something for this specific run.
//!
//! This does not cover the `CONSTRUCT_*` knobs that select paths and assets
//! — those are read while locating the config file, before there is a table
//! to consult, and must stay real environment.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

fn overlay() -> &'static RwLock<HashMap<String, String>> {
    static OVERLAY: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    OVERLAY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install `[daemon.env]` as the fallback layer. Called once at daemon
/// startup and again by `doctor`, which loads the same config with no
/// daemon running — the two must resolve credentials identically or the
/// diagnosis describes a machine the daemon isn't on (spec 0168).
pub fn install<I, K, V>(pairs: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut map: HashMap<String, String> = pairs
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    map.retain(|k, v| !k.trim().is_empty() && !v.trim().is_empty());
    *overlay().write().unwrap_or_else(|e| e.into_inner()) = map;
}

/// Resolve `name`: the real environment first, then `[daemon.env]`.
/// Empty and whitespace-only values count as unset on both layers — a
/// blank key is a missing key everywhere else in the credential paths.
pub fn var(name: &str) -> Option<String> {
    if let Some(v) = std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Some(v);
    }
    overlay()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Whether `name` resolves to a non-empty value on either layer.
pub fn present(name: &str) -> bool {
    var(name).is_some()
}

/// The pairs a spawned session should start from: every `[daemon.env]`
/// entry the child would not already inherit. Entries the real environment
/// already provides are left out, so the child sees exactly what this
/// module would resolve — one precedence rule, both directions.
pub fn child_env_base() -> HashMap<String, String> {
    overlay()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(k, _)| {
            !std::env::var(k.as_str())
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with the overlay set to `pairs` and `name` forced to `value`
    /// in the real environment, restoring both afterwards. Takes the
    /// crate-wide env guard: the process environment and this overlay are
    /// both global while tests run in parallel.
    fn with_env<T>(
        name: &str,
        value: Option<&str>,
        pairs: &[(&str, &str)],
        f: impl FnOnce() -> T,
    ) -> T {
        let _lock = crate::router::oauth::test_env_guard();
        let saved = std::env::var(name).ok();
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        install(pairs.iter().map(|(k, v)| (*k, *v)));
        let out = f();
        install(Vec::<(String, String)>::new());
        match saved {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        out
    }

    #[test]
    fn config_fills_a_gap_in_the_real_environment() {
        with_env("CONSTRUCT_TEST_KEY", None, &[("CONSTRUCT_TEST_KEY", "from-config")], || {
            assert_eq!(var("CONSTRUCT_TEST_KEY").as_deref(), Some("from-config"));
            assert!(present("CONSTRUCT_TEST_KEY"));
        });
    }

    #[test]
    fn the_real_environment_wins_over_config() {
        with_env(
            "CONSTRUCT_TEST_KEY",
            Some("from-shell"),
            &[("CONSTRUCT_TEST_KEY", "from-config")],
            || {
                assert_eq!(var("CONSTRUCT_TEST_KEY").as_deref(), Some("from-shell"));
            },
        );
    }

    /// A variable exported as empty is not a value — config still fills it,
    /// matching how every credential path treats a blank key.
    #[test]
    fn an_empty_real_value_does_not_shadow_config() {
        with_env(
            "CONSTRUCT_TEST_KEY",
            Some("   "),
            &[("CONSTRUCT_TEST_KEY", "from-config")],
            || {
                assert_eq!(var("CONSTRUCT_TEST_KEY").as_deref(), Some("from-config"));
            },
        );
    }

    #[test]
    fn a_blank_config_value_is_not_a_value() {
        with_env("CONSTRUCT_TEST_KEY", None, &[("CONSTRUCT_TEST_KEY", "  ")], || {
            assert_eq!(var("CONSTRUCT_TEST_KEY"), None);
            assert!(!present("CONSTRUCT_TEST_KEY"));
        });
    }

    /// Children inherit the real environment on their own, so the base map
    /// carries only what config adds — never a value that would override
    /// what the user exported.
    #[test]
    fn child_base_carries_config_only_where_the_shell_is_silent() {
        with_env(
            "CONSTRUCT_TEST_KEY",
            Some("from-shell"),
            &[("CONSTRUCT_TEST_KEY", "from-config"), ("CONSTRUCT_TEST_OTHER", "only-config")],
            || {
                let base = child_env_base();
                assert_eq!(base.get("CONSTRUCT_TEST_OTHER").map(String::as_str), Some("only-config"));
                assert!(
                    !base.contains_key("CONSTRUCT_TEST_KEY"),
                    "an exported value must not be overridden by config: {base:?}"
                );
            },
        );
    }
}
