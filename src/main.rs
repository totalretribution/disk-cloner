mod backup;
mod clone;
mod lsblk;
mod manifest;
mod restore;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Image a whole SSD/disk (partition table + every partition) using partclone.
#[derive(Parser)]
#[command(name = "disk-cloner", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List block devices so you can pick a source/target.
    List,

    /// Create a complete image of a disk into a directory.
    Backup(backup::BackupArgs),

    /// Restore a previously created image onto a disk.
    Restore(restore::RestoreArgs),

    /// Clone one disk directly onto another (no intermediate image).
    Clone(clone::CloneArgs),

    /// Show the manifest of an existing image directory.
    Info {
        /// Image directory containing manifest.json.
        dir: std::path::PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::List => lsblk::print_list(),
        Command::Backup(args) => backup::run(args),
        Command::Restore(args) => restore::run(args),
        Command::Clone(args) => clone::run(args),
        Command::Info { dir } => {
            let m = manifest::Manifest::load(&dir)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            Ok(())
        }
    }
}
