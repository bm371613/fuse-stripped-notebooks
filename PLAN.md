# Plan: fuse-stripped-notebooks

## Context

Build a read-only FUSE filesystem in Rust (`fuser` crate) that mirrors a source directory but transforms `.ipynb` files on the fly. Two modes are supported, selectable via CLI flag:

1. **strip-outputs** (default): return the notebook JSON with outputs/execution_count removed
2. **python-script**: convert the notebook to a Python script (code cells as-is + `# ---`, markdown/raw cells as triple-quoted docstrings)

All other files pass through unchanged. Paths stay the same — `.ipynb` extension is kept.

## File layout

```
Cargo.toml
src/
  main.rs        — CLI (clap derive), calls fuser::mount2
  fs.rs          — NotebookFs: inode table, cache, all FUSE ops
  transform.rs   — pure functions: strip_outputs / to_python_script
```

---

## Cargo.toml

Bootstrap with `cargo init`, then add dependencies at their current latest versions:

```
cargo add fuser serde_json libc log env_logger
cargo add clap --features derive
```

No reason to pin to specific versions — Cargo picks the latest compatible release.

---

## src/transform.rs

Two pure functions, no FUSE types.

### `strip_outputs(raw: &[u8]) -> Result<Vec<u8>, ...>`
- Parse JSON, walk `value["cells"]`, set `outputs = []`, `execution_count = null`
- Re-serialize with `serde_json::to_vec`

### `to_python_script(raw: &[u8]) -> Result<Vec<u8>, ...>`
- Parse JSON, iterate cells
- `source` field is either `Value::String` (split on `\n` with `split_inclusive`) or `Value::Array` of strings
- `markdown`/`raw` cells → `"""\n` + lines stripped of trailing `\n` + `"""\n`
- `code` cells → lines stripped of trailing `\n` + `# ---\n`
- Unknown cell types: skip

---

## src/fs.rs

### Data structures

```rust
pub enum Mode { StripOutputs, PythonScript }

pub struct NotebookFs {
    source: PathBuf,
    mode: Mode,
    inner: Mutex<FsState>,
}

struct FsState {
    next_ino: u64,                                  // starts at 2; INodeNo::ROOT=INodeNo(1) pre-registered
    inodes: HashMap<u64, InodeEntry>,               // ino (raw u64) → entry
    path_index: HashMap<PathBuf, u64>,              // real_path → ino (dedup on revisit)
    content_cache: HashMap<u64, Arc<Vec<u8>>>,      // ino → transformed bytes (ipynb only)
    open_files: HashMap<u64, File>,                 // fh (raw u64) → open File (passthrough reads)
    next_fh: u64,
}

struct InodeEntry {
    real_path: PathBuf,
    kind: FileType,   // Directory | RegularFile | Symlink
}
```

Root is pre-registered: `inodes[1] = InodeEntry { source.clone(), Directory }`.
The root inode is `INodeNo::ROOT` (= `INodeNo(1)`); use `.0` to get the raw `u64` for map lookups.

### Key helpers

**`assign_or_lookup_ino(state, real_path)`** — uses `fs::symlink_metadata` (must not follow symlinks), returns `(ino, kind)`, creates entry if new.

**`is_notebook(path)`** — `path.extension() == "ipynb"`.

**`get_or_transform(state, ino, path, mode)`** — checks cache, else reads file, calls transform fn, stores in cache, returns `Arc<Vec<u8>>`.

**`metadata_to_fileattr(ino: u64, meta, size_override)`** — uses `std::os::unix::fs::MetadataExt` for atime/mtime/ctime/mode/nlink/uid/gid/rdev/blksize. `FileAttr.ino` is `INodeNo`, so set it as `INodeNo(ino)`. Strip write bits: `(meta.mode() as u16) & 0o555`. Set `crtime = ctime` (Linux ignores crtime). The `flags` field is a plain `u32` — set to `0`.

**Notebook size & cache priming** — for `.ipynb` files, `lookup` and `getattr` must report the *transformed* size (the kernel uses it to bound `read`). Run the transform during `lookup` and store the result in `content_cache` so `getattr` and `read` reuse it without re-parsing. Yes, this means every stat on a notebook does a full transform; that's the price of an accurate size on a synthesized file.

**Transform errors** — `strip_outputs` / `to_python_script` return `Result<Vec<u8>, …>`. On failure, the relevant FUSE op replies with `reply.error(Errno::EIO)`. Never panic inside a FUSE handler — it kills the mounted filesystem.

### FUSE ops implemented

All trait methods receive newtypes — `INodeNo`, `FileHandle`, `OpenFlags` — not raw `u64`/`i32`. Extract the inner value with `.0` when indexing into `HashMap<u64, _>`.

| Op | Behaviour |
|----|-----------|
| `lookup(parent: INodeNo, name)` | Build child path, `symlink_metadata`, `assign_or_lookup_ino`, compute transform size for ipynb, `reply.entry(&TTL, &attr, Generation(0))` |
| `getattr(ino: INodeNo, fh: Option<FileHandle>)` | `symlink_metadata` on stored path, transform size for ipynb, `reply.attr(&TTL, &attr)` |
| `readdir(ino: INodeNo, fh: FileHandle, offset: u64)` | `fs::read_dir`, prepend `.`/`..`, assign inodes, emit entries starting from `offset`; pass `INodeNo(child_ino)` and `(i+1) as u64` as next-offset cookie to `reply.add`. `reply.add` is `#[must_use]` and returns `true` when the kernel buffer is full — break out of the loop immediately when it does. |
| `open(ino: INodeNo, flags: OpenFlags)` | Reject if `flags.acc_mode() != OpenAccMode::O_RDONLY` → `reply.error(Errno::EROFS)`. For ipynb: cache transform, `reply.opened(FileHandle(0), FopenFlags::empty())`. For others: `File::open`, store by new fh, `reply.opened(FileHandle(next_fh), FopenFlags::empty())` |
| `read(ino: INodeNo, fh: FileHandle, offset: u64, size: u32, ...)` | ipynb: clone `Arc`, slice `[offset as usize..(offset+size as u64) as usize]`. Passthrough: seek+read from `open_files[fh.0]` |
| `release(ino: INodeNo, fh: FileHandle, ...)` | `open_files.remove(&fh.0)`, `reply.ok()` |
| `opendir` | `reply.opened(FileHandle(0), FopenFlags::empty())` |
| `releasedir` | `reply.ok()` |
| `readlink(ino: INodeNo)` | Clone path, drop lock, `fs::read_link`, `reply.data(target.as_os_str().as_bytes())` |

Write operations (`write`, `mkdir`, `unlink`, etc.) use the default `ENOSYS` from the trait — the `MountOption::RO` mount flag blocks them at the kernel level before they reach us anyway.

### TTL
`const TTL: Duration = Duration::from_secs(1);` used for all `reply.entry` / `reply.attr` calls.

---

## src/main.rs

Key imports from fuser:

```rust
use fuser::{
    Config, Errno, FileAttr, FileType, Filesystem, MountOption,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyEmpty,
    Request, INodeNo, FileHandle, Generation, OpenFlags, OpenAccMode, FopenFlags,
};
```

```rust
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
    // mount2 takes &Config (not &[MountOption]). Config is #[non_exhaustive],
    // so build via Default + field mutation rather than a struct literal.
    let mut cfg = Config::default();
    cfg.mount_options.push(MountOption::RO);
    cfg.mount_options.push(MountOption::FSName("notebookfs".to_string()));
    fuser::mount2(NotebookFs::new(args.source, args.mode), &args.mountpoint, &cfg).unwrap();
}
```

---

## Pitfalls to watch

- Always `symlink_metadata`, never `metadata`, when detecting file type — `metadata` follows symlinks
- fuser uses newtypes: `INodeNo(u64)`, `FileHandle(u64)`, `Generation(u64)`, `OpenFlags(i32)`. Extract inner values with `.0` when indexing `HashMap<u64, _>`
- Check write access with `flags.acc_mode() != OpenAccMode::O_RDONLY` via `OpenFlags::acc_mode()`
- Root inode is `INodeNo::ROOT` (= `INodeNo(1)`); use `.0` for raw map keys
- `reply.entry` third arg is `Generation(0)`
- `reply.opened` takes `(FileHandle(fh), FopenFlags::empty())`
- `FileAttr.ino` is `INodeNo` — wrap with `INodeNo(ino)`
- `perm` in `FileAttr` is lower 12 bits only — `kind` is separate; don't include file-type bits
- Release `MutexGuard` before blocking I/O where possible (especially `readlink`)
- `readdir` offset is 1-based: entry at vector index `i` gets next-offset `(i+1) as u64`. `reply.add` is `#[must_use]` — break the loop when it returns `true` (buffer full)
- `Generation(0)` in `reply.entry` is correct for a non-persistent fs
- `fuser::mount2` takes `&Config`, not `&[MountOption]` — see main.rs sketch above
- Don't panic inside a FUSE handler; reply with `Errno::EIO` (or another appropriate errno) instead, otherwise the mount becomes unusable

---

## Verification

```bash
# Build
cargo build

# Setup test data
mkdir -p /tmp/src /tmp/mnt
cp some_notebook.ipynb /tmp/src/
echo "hello" > /tmp/src/regular.txt

# Mount (strip-outputs mode)
./target/debug/fuse-stripped-notebooks /tmp/src /tmp/mnt

# Check
ls /tmp/mnt
cat /tmp/mnt/regular.txt        # should match original
cat /tmp/mnt/some_notebook.ipynb  # should be JSON without outputs

# Mount (python-script mode)
fusermount -u /tmp/mnt
./target/debug/fuse-stripped-notebooks --mode python-script /tmp/src /tmp/mnt
cat /tmp/mnt/some_notebook.ipynb  # should be Python script

# Unmount
fusermount -u /tmp/mnt
```
