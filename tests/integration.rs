//! Integration tests for the FUSE filesystem.
//!
//! Each test mounts its own instance of the binary into a fresh tempdir, exercises
//! it via plain `std::fs` calls, then unmounts on `Drop`.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn copy_fixture(name: &str, into: &Path) {
    fs::copy(fixture(name), into.join(name)).unwrap();
}

struct MountedFs {
    child: Child,
    mountpoint: tempfile::TempDir,
    source: tempfile::TempDir,
}

impl MountedFs {
    fn new(mode: &str, populate: impl FnOnce(&Path)) -> Self {
        let source = tempfile::tempdir().unwrap();
        populate(source.path());

        let mountpoint = tempfile::tempdir().unwrap();
        let bin = env!("CARGO_BIN_EXE_fuse-stripped-notebooks");
        let child = Command::new(bin)
            .arg("--mode")
            .arg(mode)
            .arg("--source")
            .arg(source.path())
            .arg("--mountpoint")
            .arg(mountpoint.path())
            .spawn()
            .expect("failed to spawn fuse binary");

        wait_mounted(mountpoint.path());
        Self { child, mountpoint, source }
    }

    fn mnt(&self) -> &Path {
        self.mountpoint.path()
    }

    fn src(&self) -> &Path {
        self.source.path()
    }
}

impl Drop for MountedFs {
    fn drop(&mut self) {
        unmount(self.mountpoint.path());
        // Give the binary up to 2s to exit gracefully after unmount.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_mounted(mnt: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if is_mountpoint(mnt) && fs::read_dir(mnt).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("FUSE mount at {mnt:?} never became ready within 10s");
}

fn is_mountpoint(p: &Path) -> bool {
    let Ok(canon) = p.canonicalize() else {
        return false;
    };
    let target = canon.to_string_lossy();
    fs::read_to_string("/proc/self/mountinfo")
        .map(|content| {
            content
                .lines()
                .any(|line| line.split_whitespace().nth(4).is_some_and(|m| m == target))
        })
        .unwrap_or(false)
}

fn unmount(p: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        if let Ok(status) = Command::new(tool).arg("-u").arg(p).status()
            && status.success()
        {
            return;
        }
    }
    eprintln!("warning: failed to unmount {p:?}");
}

// Expected output for the python-script transform of sample.ipynb.
// Markdown cells become triple-quoted blocks; code cells get `# ---` separators.
const EXPECTED_PYTHON_SCRIPT: &str =
    "\"\"\"\n# Test notebook\nSample for integration tests.\n\"\"\"\nmsg = \"hello\"\nprint(msg)\n# ---\n";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn lists_directory_entries() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.txt", src);
        copy_fixture("sample.ipynb", src);
    });
    let mut names: Vec<String> = fs::read_dir(fs_.mnt())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["sample.ipynb", "sample.txt"]);
}

#[test]
fn txt_size_and_content_unchanged() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.txt", src);
    });
    let original = fs::read(fixture("sample.txt")).unwrap();
    let mnt_path = fs_.mnt().join("sample.txt");

    let mnt_meta = fs::metadata(&mnt_path).unwrap();
    assert_eq!(
        mnt_meta.len() as usize,
        original.len(),
        "txt size must match the source"
    );

    let mnt_bytes = fs::read(&mnt_path).unwrap();
    assert_eq!(mnt_bytes, original, "txt contents must be byte-identical");
}

#[test]
fn ipynb_strip_size_matches_read_and_drops_outputs() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.ipynb", src);
    });
    let p = fs_.mnt().join("sample.ipynb");

    let bytes = fs::read(&p).unwrap();
    let stat_size = fs::metadata(&p).unwrap().len() as usize;
    assert_eq!(
        stat_size,
        bytes.len(),
        "size reported by stat must match bytes actually returned"
    );

    let source_size = fs::metadata(fixture("sample.ipynb")).unwrap().len() as usize;
    assert!(
        stat_size < source_size,
        "stripped ipynb ({stat_size} B) should be smaller than the source ({source_size} B)"
    );

    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("stripped ipynb is valid JSON");
    let cells = json["cells"].as_array().expect("cells is an array");
    let mut saw_code_cell = false;
    for cell in cells {
        if cell["cell_type"] == "code" {
            saw_code_cell = true;
            assert_eq!(
                cell["outputs"],
                serde_json::json!([]),
                "code-cell outputs must be empty after strip"
            );
            assert!(
                cell["execution_count"].is_null(),
                "code-cell execution_count must be null after strip"
            );
        }
    }
    assert!(saw_code_cell, "fixture should include at least one code cell");
}

#[test]
fn ipynb_python_script_mode_produces_expected_bytes() {
    let fs_ = MountedFs::new("python-script", |src| {
        copy_fixture("sample.ipynb", src);
    });
    let p = fs_.mnt().join("sample.ipynb");

    let bytes = fs::read(&p).unwrap();
    let stat_size = fs::metadata(&p).unwrap().len() as usize;
    assert_eq!(stat_size, bytes.len(), "stat size must match read length");

    let actual = std::str::from_utf8(&bytes).expect("python script is UTF-8");
    assert_eq!(actual, EXPECTED_PYTHON_SCRIPT);
}

#[test]
fn write_to_txt_is_rejected() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.txt", src);
    });
    let err = fs::OpenOptions::new()
        .write(true)
        .open(fs_.mnt().join("sample.txt"))
        .expect_err("opening a txt for write should fail on a read-only mount");
    assert_rejects_write(&err);
}

#[test]
fn write_to_ipynb_is_rejected() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.ipynb", src);
    });
    let err = fs::OpenOptions::new()
        .write(true)
        .open(fs_.mnt().join("sample.ipynb"))
        .expect_err("opening an ipynb for write should fail on a read-only mount");
    assert_rejects_write(&err);
}

fn assert_rejects_write(err: &std::io::Error) {
    // The kernel turns writes against a RO mount into EROFS; some configurations
    // may surface it as EACCES at the access-check stage. Either is acceptable.
    let raw = err.raw_os_error();
    assert!(
        raw == Some(libc::EROFS) || raw == Some(libc::EACCES),
        "expected EROFS or EACCES, got errno={raw:?} ({err})"
    );
}

#[test]
fn preserves_uid_gid_and_timestamps() {
    let fs_ = MountedFs::new("strip-outputs", |src| {
        copy_fixture("sample.txt", src);
        copy_fixture("sample.ipynb", src);
    });

    for name in ["sample.txt", "sample.ipynb"] {
        let src_meta = fs::symlink_metadata(fs_.src().join(name)).unwrap();
        let mnt_meta = fs::symlink_metadata(fs_.mnt().join(name)).unwrap();
        assert_eq!(src_meta.uid(), mnt_meta.uid(), "uid mismatch for {name}");
        assert_eq!(src_meta.gid(), mnt_meta.gid(), "gid mismatch for {name}");
        assert_eq!(
            src_meta.mtime(),
            mnt_meta.mtime(),
            "mtime mismatch for {name}"
        );
        assert_eq!(
            src_meta.mtime_nsec(),
            mnt_meta.mtime_nsec(),
            "mtime nsec mismatch for {name}"
        );
        // PLAN.md sets crtime = ctime on Linux; we verify ctime is propagated
        // from the source so the synthesised "creation time" follows the inode.
        assert_eq!(
            src_meta.ctime(),
            mnt_meta.ctime(),
            "ctime mismatch for {name}"
        );
    }
}

#[test]
fn unreadable_source_stays_unreadable_through_the_mount() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping unreadable-file test: running as root bypasses mode checks");
        return;
    }
    let fs_ = MountedFs::new("strip-outputs", |src| {
        let p = src.join("secret.txt");
        fs::write(&p, b"top secret").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    });

    let err = fs::read(fs_.mnt().join("secret.txt"))
        .expect_err("reading a mode-0 file should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::PermissionDenied,
        "expected EACCES, got {err:?}"
    );
}

