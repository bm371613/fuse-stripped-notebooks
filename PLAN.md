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
    next_ino: u64,                             // starts at 2; FUSE_ROOT_ID=1 pre-registered
    inodes: HashMap<u64, InodeEntry>,          // ino → entry
    path_index: HashMap<PathBuf, u64>,         // real_path → ino (dedup on revisit)
    content_cache: HashMap<u64, Arc<Vec<u8>>>, // ino → transformed bytes (ipynb only)
    open_files: HashMap<u64, File>,            // fh → open File (passthrough reads)
    next_fh: u64,
}

struct InodeEntry {
    real_path: PathBuf,
    kind: FileType,   // Directory | RegularFile | Symlink
}
```

Root is pre-registered: `inodes[FUSE_ROOT_ID] = InodeEntry { source.clone(), Directory }`.

### Key helpers

**`assign_or_lookup_ino(state, real_path)`** — uses `fs::symlink_metadata` (must not follow symlinks), returns `(ino, kind)`, creates entry if new.

**`is_notebook(path)`** — `path.extension() == "ipynb"`.

**`get_or_transform(state, ino, path, mode)`** — checks cache, else reads file, calls transform fn, stores in cache, returns `Arc<Vec<u8>>`.

**`metadata_to_fileattr(ino, meta, size_override)`** — uses `std::os::unix::fs::MetadataExt` for atime/mtime/ctime/mode/nlink/uid/gid/rdev/blksize. Strip write bits: `(meta.mode() as u16) & 0o555`. Set `crtime = ctime` (Linux ignores crtime).

### FUSE ops implemented

| Op | Behaviour |
|----|-----------|
| `lookup(parent, name)` | Build child path, `symlink_metadata`, `assign_or_lookup_ino`, compute transform size for ipynb, `reply.entry(&TTL, &attr, 0)` |
| `getattr(ino)` | `symlink_metadata` on stored path, transform size for ipynb, `reply.attr` |
| `readdir(ino, offset)` | `fs::read_dir`, prepend `.`/`..`, assign inodes, emit entries starting from `offset`; pass `(i+1)` as next-offset cookie to `reply.add` |
| `open(ino, flags)` | Reject if `flags & O_ACCMODE != O_RDONLY` → `EROFS`. For ipynb: cache transform, `reply.opened(0, 0)`. For others: `File::open`, store by new fh |
| `read(ino, fh, offset, size)` | ipynb: clone `Arc`, slice `[offset..offset+size]`. Passthrough: seek+read from `open_files[fh]` |
| `release` | `open_files.remove(fh)` |
| `opendir` | `reply.opened(0, 0)` |
| `releasedir` | `reply.ok()` |
| `readlink(ino)` | Clone path, drop lock, `fs::read_link`, `reply.data(target.as_bytes())` |

Write operations (`write`, `mkdir`, `unlink`, etc.) use the default `ENOSYS` from the trait — the `MountOption::RO` mount flag blocks them at the kernel level before they reach us anyway.

### TTL
`const TTL: Duration = Duration::from_secs(1);` used for all `reply.entry` / `reply.attr` calls.

---

## src/main.rs

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
    let options = vec![MountOption::RO, MountOption::FSName("notebookfs".to_string())];
    fuser::mount2(NotebookFs::new(args.source, args.mode), args.mountpoint, &options).unwrap();
}
```

---

## Pitfalls to watch

- Always `symlink_metadata`, never `metadata`, when detecting file type — `metadata` follows symlinks
- `O_RDONLY = 0`, check write access with `flags & libc::O_ACCMODE != libc::O_RDONLY`
- `perm` in `FileAttr` is lower 12 bits only — `kind` is separate; don't include file-type bits
- Release `MutexGuard` before blocking I/O where possible (especially `readlink`)
- `readdir` offset is 1-based: entry at vector index `i` gets next-offset `i+1`
- `generation = 0` in `reply.entry` is correct for a non-persistent fs

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
