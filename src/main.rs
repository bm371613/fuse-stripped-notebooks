use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use clap::Parser;
use fuser::{Config, MountOption};

mod fs;
mod transform;

use fs::{Mode, NotebookFs};

static SIGNAL_RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn signal_handler(sig: libc::c_int) {
    SIGNAL_RECEIVED.store(sig, Ordering::Relaxed);
}

#[derive(Parser)]
struct Cli {
    #[arg(long, value_enum, default_value = "python-script")]
    mode: Mode,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    mountpoint: PathBuf,
    #[arg(long)]
    mkdir: bool,
}

fn main() {
    env_logger::init();
    let args = Cli::parse();

    let created_mountpoint = if args.mkdir && !args.mountpoint.exists() {
        std::fs::create_dir(&args.mountpoint).expect("Failed to create mountpoint");
        true
    } else {
        false
    };
    let mut cfg = Config::default();
    cfg.mount_options.push(MountOption::RO);
    cfg.mount_options
        .push(MountOption::FSName("fuse-stripped-notebooks".to_string()));

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as libc::sighandler_t;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }

    let session = fuser::spawn_mount2(
        NotebookFs::new(args.source, args.mode),
        &args.mountpoint,
        &cfg,
    )
    .unwrap();

    let externally_unmounted = loop {
        let sig = SIGNAL_RECEIVED.load(Ordering::Relaxed);
        if sig != 0 {
            let name = unsafe { CStr::from_ptr(libc::strsignal(sig)) }.to_string_lossy();
            eprintln!("\nReceived signal: {name}");
            break false;
        }
        if session.guard.is_finished() {
            break true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    if externally_unmounted {
        eprintln!("Unmounted externally.");
        std::mem::forget(session);
    } else {
        drop(session);
        eprintln!("Unmounted.");
    }
    if created_mountpoint {
        if let Err(e) = std::fs::remove_dir(&args.mountpoint) {
            eprintln!("Warning: could not remove mountpoint: {e}");
        }
    }
}
