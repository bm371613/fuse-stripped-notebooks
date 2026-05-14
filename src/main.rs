use std::path::PathBuf;

use clap::Parser;
use fuser::{Config, MountOption};

mod fs;
mod transform;

use fs::{Mode, NotebookFs};

#[derive(Parser)]
struct Cli {
    #[arg(long, value_enum, default_value = "strip-outputs")]
    mode: Mode,
    source: PathBuf,
    mountpoint: PathBuf,
}

fn main() {
    env_logger::init();
    let args = Cli::parse();
    let mut cfg = Config::default();
    cfg.mount_options.push(MountOption::RO);
    cfg.mount_options.push(MountOption::FSName("notebookfs".to_string()));
    fuser::mount2(NotebookFs::new(args.source, args.mode), &args.mountpoint, &cfg).unwrap();
}
