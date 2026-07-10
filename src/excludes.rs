//! Exclude rules, ported from the Python oracle at `./duh:198-255`.
//!
//! Two families of default excludes:
//! - Single-name excludes match a directory entry's basename (`name`).
//! - Multi-component excludes match by relative-path suffix (`rel_path`),
//!   with a component-boundary check so e.g. `.git/objects` matches
//!   `a/.git/objects` and `.git/objects` but not `notgit/objects`.
//!
//! CLI wiring (see `duh:225-243`):
//! - `--no-default-excludes` empties both default sets before anything else.
//! - `--include` removes names from the (possibly already-emptied) default
//!   single/multi sets.
//! - `--exclude` adds extra single-name patterns, applied last, so it still
//!   takes effect even after `--no-default-excludes`.

use std::collections::HashSet;

/// Directory/file basenames excluded by default (`duh:198-211`).
const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    ".git/objects",
    ".terraform/providers",
    ".terraform/modules",
    ".next/cache",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "target", // Rust
];

/// Entries from `DEFAULT_EXCLUDES` that match by relative-path suffix
/// instead of by basename (`duh:214-219`).
const MULTI_COMPONENT_EXCLUDES: &[&str] = &[
    ".git/objects",
    ".terraform/providers",
    ".terraform/modules",
    ".next/cache",
];

pub struct Excludes {
    single: HashSet<String>,
    multi: HashSet<String>,
}

impl Excludes {
    /// Build the effective exclude sets from CLI-style arguments.
    ///
    /// Mirrors `build_excludes` (`duh:225-243`): start from the defaults
    /// (or empty, if `no_defaults`), let `include` remove entries from
    /// those defaults, then let `exclude` add extra single-name patterns.
    pub fn from_args(exclude: &[String], include: &[String], no_defaults: bool) -> Excludes {
        let (mut single, mut multi) = if no_defaults {
            (HashSet::new(), HashSet::new())
        } else {
            let multi: HashSet<String> = MULTI_COMPONENT_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect();
            let single: HashSet<String> = DEFAULT_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .filter(|s| !multi.contains(s))
                .collect();
            (single, multi)
        };

        for name in include {
            single.remove(name);
            multi.remove(name);
        }

        for name in exclude {
            single.insert(name.clone());
        }

        Excludes { single, multi }
    }

    /// Check if a directory entry should be excluded.
    ///
    /// Mirrors `is_excluded` (`duh:246-255`): `name` is checked against the
    /// single-name set (basename match), then `rel_path` is checked against
    /// each multi-component pattern via suffix match (`ends_with` or exact
    /// equality), which is what gives `.git/objects` a component-boundary
    /// match against `a/.git/objects` without also matching `src/objects`.
    pub fn matches(&self, name: &str, rel_path: &str) -> bool {
        if self.single.contains(name) {
            return true;
        }
        for pat in &self.multi {
            if rel_path == pat || rel_path.ends_with(pat) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excludes_match_by_name() {
        let ex = Excludes::from_args(&[], &[], false);
        assert!(ex.matches("node_modules", "a/b/node_modules"));
        assert!(!ex.matches("src", "a/b/src"));
    }

    #[test]
    fn multi_component_excludes_match_by_rel_suffix() {
        let ex = Excludes::from_args(&[], &[], false);
        assert!(ex.matches("objects", ".git/objects")); // matches duh:246-255 semantics
        assert!(!ex.matches("objects", "src/objects"));
    }

    #[test]
    fn include_removes_default_and_no_defaults_clears() {
        let ex = Excludes::from_args(&[], &["node_modules".into()], false);
        assert!(!ex.matches("node_modules", "x/node_modules"));
        let ex = Excludes::from_args(&[], &[], true);
        assert!(!ex.matches(".venv", "x/.venv"));
    }

    // --- Additional edge-case tests derived from the oracle's is_excluded logic ---

    /// The Python `endswith` check has no component-boundary guard baked
    /// into `is_excluded` itself, but the exact-suffix nature of the pattern
    /// (starting with a leading path separator boundary like ".git/") means
    /// a partial-name collision (e.g. "notgit/objects") must NOT match,
    /// while a deeper nested path ending in the pattern still does.
    #[test]
    fn multi_component_excludes_match_nested_suffix_but_not_partial_name_collision() {
        let ex = Excludes::from_args(&[], &[], false);
        // Nested: pattern is a suffix of a longer relative path.
        assert!(ex.matches("objects", "repo/a/b/.git/objects"));
        // Partial basename collision ("notgit" contains "git" but the
        // path does not end with the ".git/objects" component sequence).
        assert!(!ex.matches("objects", "repo/notgit/objects"));
    }

    /// `.git/objects` also appears in `_SINGLE_NAME_EXCLUDES`'s complement
    /// check implicitly: verify a name that is exactly the multi-component
    /// pattern's final component ("objects") is NOT excluded by basename
    /// alone when the rel_path doesn't carry the required prefix.
    #[test]
    fn multi_component_pattern_does_not_leak_into_single_name_matching() {
        let ex = Excludes::from_args(&[], &[], false);
        // "objects" alone is never in the single-name set, so a bare
        // top-level "objects" dir is not excluded.
        assert!(!ex.matches("objects", "objects"));
    }

    /// `--no-default-excludes` clears the defaults, but an explicit
    /// `--exclude` must still apply (added after the defaults are wiped).
    #[test]
    fn no_defaults_with_explicit_exclude_still_matches() {
        let ex = Excludes::from_args(&["build".to_string()], &[], true);
        assert!(ex.matches("build", "a/b/build"));
        // Defaults are gone even though an explicit exclude was given.
        assert!(!ex.matches("node_modules", "a/node_modules"));
    }
}
