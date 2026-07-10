#[cfg(not(target_os = "macos"))]
compile_error!("duh is macOS-only");

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// macOS APFS-aware disk-usage database (v2). Detects clones and hardlinks.
#[derive(Parser)]
#[command(name = "duh", version)]
struct Cli {
    /// Override DB path (or set DUH_DB env var)
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify APFS clone detection (ctypes getattrlist test)
    Selftest,

    /// Walk a directory and index files
    Scan {
        path: String,
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Skip clone_id detection (faster)
        #[arg(long)]
        no_clones: bool,
        /// Delete existing scan for this root before scanning
        #[arg(long)]
        rescan: bool,
        /// Follow mounts into other filesystems
        #[arg(long)]
        cross_device: bool,
        /// Abort scan if free disk drops below N GiB (default 3.0)
        #[arg(long, value_name = "GIB", default_value_t = 3.0)]
        min_free: f64,
        /// Add a directory name to the exclusion list (repeatable)
        #[arg(long, value_name = "NAME")]
        exclude: Vec<String>,
        /// Remove a name from the default exclusion list (repeatable)
        #[arg(long, value_name = "NAME")]
        include: Vec<String>,
        /// Don't apply the default exclusion list
        #[arg(long)]
        no_default_excludes: bool,
    },

    /// Show top directories by size
    Top {
        /// Limit to this path prefix
        #[arg(long)]
        under: Option<String>,
        #[arg(long, default_value = "blocks", value_parser = ["blocks", "logical", "clones"])]
        by: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        #[arg(short = 'd', long, default_value_t = 1)]
        depth: i64,
        #[arg(long)]
        json: bool,
    },

    /// Show marginal disk cost of a path
    Marginal {
        path: String,
        #[arg(long)]
        json: bool,
    },

    /// Show metadata for a single file
    File { path: String },

    /// Rank clone families by waste
    Clones {
        #[arg(long)]
        under: Option<String>,
        #[arg(long, value_name = "N", default_value_t = 1024 * 1024)]
        min_bytes: i64,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },

    /// List excluded subtrees with aggregate sizes
    Excluded {
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: i64,
    },

    /// Report top delete-together groups by locked-at size
    Clusters {
        #[arg(long, value_name = "N", default_value_t = 100 * 1024 * 1024)]
        min_bytes: i64,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },

    /// Show precomputed freeable bytes for a path
    Freeable { path: String },

    /// Open DB in sqlite3 with helpful views
    Sql,

    /// Show summary of last scan
    Stats,

    /// Launch local web UI (treesize-style disk explorer)
    Serve {
        /// Starting port (tries up to port+10 if taken, default 7777)
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Don't auto-open browser
        #[arg(long)]
        no_browser: bool,
    },
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Selftest => "selftest",
            Command::Scan { .. } => "scan",
            Command::Top { .. } => "top",
            Command::Marginal { .. } => "marginal",
            Command::File { .. } => "file",
            Command::Clones { .. } => "clones",
            Command::Excluded { .. } => "excluded",
            Command::Clusters { .. } => "clusters",
            Command::Freeable { .. } => "freeable",
            Command::Sql => "sql",
            Command::Stats => "stats",
            Command::Serve { .. } => "serve",
        }
    }
}

/// Resolve the DB path: `--db` flag > `DUH_DB` env var > `~/.local/share/duh/scan.db`.
fn resolve_db_path(db_flag: Option<PathBuf>) -> PathBuf {
    db_flag.unwrap_or_else(duh::db::default_db_path)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _db_path = resolve_db_path(cli.db);

    // No subcommand is implemented yet; later tasks will wire each one up.
    eprintln!("{}: not yet ported", cli.command.name());
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both assertions live in one test (rather than two) because std::env::set_var
    // mutates process-global state; separate #[test] fns run on different threads
    // and would race on DUH_DB.
    #[test]
    fn resolve_db_path_precedence() {
        std::env::set_var("DUH_DB", "/tmp/from-env.db");
        let resolved = resolve_db_path(Some(PathBuf::from("/tmp/from-flag.db")));
        assert_eq!(resolved, PathBuf::from("/tmp/from-flag.db"));

        let resolved = resolve_db_path(None);
        assert_eq!(resolved, PathBuf::from("/tmp/from-env.db"));

        std::env::remove_var("DUH_DB");
    }
}
