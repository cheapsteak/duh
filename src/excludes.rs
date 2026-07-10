//! Exclude rules, ported from the Python oracle at `./duh:198-255`.
//!
//! Two families of default excludes:
//! - Single-name excludes match a directory entry's basename (`name`).
//! - Multi-component excludes match by relative-path suffix (`rel_path`)
//!   as a BARE string suffix test — there is no path-component-boundary
//!   check. e.g. `.git/objects` matches `a/.git/objects` and `.git/objects`
//!   but not `notgit/objects` — yet it DOES match `x.git/objects`, an
//!   intentional false positive inherited from the Python oracle's
//!   `is_excluded` (`./duh:246-255`). Do NOT "harden" this with boundary
//!   logic: scan parity with the Python implementation depends on
//!   replicating this quirk exactly.
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
    /// each multi-component pattern via a bare string suffix match
    /// (`ends_with` or exact equality). There is no component-boundary
    /// check: `.git/objects` matches `a/.git/objects` but also `x.git/objects`
    /// — a false positive the Python oracle exhibits too. Kept as-is on
    /// purpose; scan parity requires replicating the quirk exactly.
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

    /// Pin the oracle's bare-suffix quirk: the multi-component check is a
    /// raw `ends_with` with NO component-boundary guard, so a rel_path like
    /// `x.git/objects` (directory literally named "x.git") DOES match the
    /// `.git/objects` pattern. The Python oracle (`./duh:246-255`) behaves
    /// identically. This false positive is intentional — if a future change
    /// "fixes" it with boundary logic, this test must fail and force the
    /// Python-parity conversation.
    #[test]
    fn bare_suffix_match_intentionally_hits_dotgit_lookalike_paths() {
        let ex = Excludes::from_args(&[], &[], false);
        assert!(ex.matches("objects", "x.git/objects"));
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
