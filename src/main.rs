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
    let db_path = resolve_db_path(cli.db);

    match cli.command {
        Command::Selftest => run_selftest(),
        Command::Scan {
            path,
            quiet,
            no_clones,
            rescan,
            cross_device,
            min_free,
            exclude,
            include,
            no_default_excludes,
        } => duh::scan::run(
            duh::scan::ScanArgs {
                path,
                quiet,
                no_clones,
                rescan,
                cross_device,
                min_free,
                exclude,
                include,
                no_default_excludes,
            },
            &db_path,
        ),
        // No other subcommand is implemented yet; later tasks will wire each up.
        other => {
            eprintln!("{}: not yet ported", other.name());
            ExitCode::from(2)
        }
    }
}

/// Port of the Python oracle's `cmd_selftest` (`./duh:121-169`): create a 1 MiB
/// random file under `~/tmp-duh-selftest/run-<millis>`, `cp -c` clone it, byte-copy
/// it, then confirm the clone shares the source's clone_id while the copy does not.
fn run_selftest() -> ExitCode {
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fmt(id: Option<u64>) -> String {
        id.map_or_else(|| "None".to_string(), |n| n.to_string())
    }

    println!("Running clone-detection self-test...");

    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let home_tmp = home.join("tmp-duh-selftest");
    if let Err(e) = std::fs::create_dir_all(&home_tmp) {
        eprintln!("  error: could not create {}: {e}", home_tmp.display());
        return ExitCode::from(1);
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let tmpdir = home_tmp.join(format!("run-{millis}"));
    if let Err(e) = std::fs::create_dir(&tmpdir) {
        eprintln!("  error: could not create {}: {e}", tmpdir.display());
        return ExitCode::from(1);
    }

    let src = tmpdir.join("src");
    let clone = tmpdir.join("clone");
    let copy = tmpdir.join("copy");

    let result = (|| -> std::io::Result<ExitCode> {
        // 1 MiB of random bytes, mirroring os.urandom(1 MiB).
        let mut urandom = std::fs::File::open("/dev/urandom")?;
        let mut buf = vec![0u8; 1 << 20];
        std::io::Read::read_exact(&mut urandom, &mut buf)?;
        std::fs::write(&src, &buf)?;

        // APFS clone via `cp -c`.
        let status = std::process::Command::new("cp")
            .arg("-c")
            .arg(&src)
            .arg(&clone)
            .status()?;
        if !status.success() {
            println!("  warning: cp -c returned non-zero (not on APFS?)");
        }

        // Genuine byte copy (shutil.copy2 equivalent). NOTE: std::fs::copy uses
        // macOS copyfile(), which CLONES on APFS — that would make the "copy"
        // share the source's clone_id. Writing the buffer to a fresh file
        // allocates independent blocks, matching Python's userspace read/write.
        std::fs::write(&copy, &buf)?;

        let src_id = duh::attrs::get_clone_id(&src);
        let clone_id = duh::attrs::get_clone_id(&clone);
        let copy_id = duh::attrs::get_clone_id(&copy);

        println!("  src   clone_id = {}", fmt(src_id));
        println!("  clone clone_id = {}", fmt(clone_id));
        println!("  copy  clone_id = {}", fmt(copy_id));

        let ok_clone = src_id.is_some() && src_id == clone_id;
        let ok_copy = copy_id.is_none() || copy_id != src_id;

        if ok_clone && ok_copy {
            println!("  PASS: src and clone share clone_id; copy has different clone_id");
            Ok(ExitCode::SUCCESS)
        } else if !ok_clone {
            println!("  FAIL: src and clone do NOT share clone_id — FFI plumbing issue");
            Ok(ExitCode::from(1))
        } else {
            println!(
                "  WARN: copy has same clone_id as src ({}) — unexpected",
                fmt(copy_id)
            );
            Ok(ExitCode::SUCCESS)
        }
    })();

    // Cleanup (best-effort), mirroring the Python `finally` block.
    let _ = std::fs::remove_dir_all(&tmpdir);
    let _ = std::fs::remove_dir(&home_tmp);

    result.unwrap_or_else(|e| {
        eprintln!("  error: {e}");
        ExitCode::from(1)
    })
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
