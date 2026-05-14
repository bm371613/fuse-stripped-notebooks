# fuse-stripped-notebooks

A read-only FUSE filesystem that mirrors a source directory and transforms
Jupyter notebooks (`.ipynb`) on the fly. Everything else passes through
unchanged.

Two transform modes are available:

- **`strip-outputs`** (default) — returns the notebook JSON with every cell's
  `outputs` cleared and `execution_count` set to `null`. Useful for diffing
  notebooks, code review, or feeding them to tools that choke on large embedded
  outputs.
- **`python-script`** — converts the notebook to a Python script: code cells are
  emitted as-is and separated by `# ---`; markdown/raw cells become
  triple-quoted blocks. Useful for grep, static analysis, and IDEs that don't
  speak `.ipynb`.

File names and the `.ipynb` extension are preserved, so existing tooling that
walks the directory tree sees the transformed notebooks at their normal paths.

## System requirements

General:

- Rust 1.85+ (edition 2024) and Cargo. Install via [rustup](https://rustup.rs).
- A Linux kernel with FUSE support (anything from the last decade).
- The `fusermount3` utility, used to mount and unmount FUSE filesystems.
- A C linker (only because Cargo invokes one at link time; no C code is built).

### Ubuntu / Debian

```bash
sudo apt-get update
sudo apt-get install -y fuse3 build-essential
# If you don't already have Rust:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

`fuse3` provides `fusermount3` and the kernel module configuration. You do not
need `libfuse3-dev` — the `fuser` crate this project uses talks to the kernel
FUSE device directly on Linux, no C library required.

No special group membership or sudo is needed to mount: a regular user can run
the binary against a mountpoint they own.

## Build from source

```bash
git clone <repo-url> fuse-stripped-notebooks
cd fuse-stripped-notebooks
cargo build --release
```

The binary lands at `target/release/fuse-stripped-notebooks`. To install it to
`~/.cargo/bin` (which is usually on your `PATH`):

```bash
cargo install --path .
```

## Usage

```bash
fuse-stripped-notebooks [--mode <strip-outputs|python-script>] <source> <mountpoint>
```

- `<source>` — the directory to mirror.
- `<mountpoint>` — an existing empty directory where the mirror should appear.

The process runs in the foreground and blocks until the filesystem is
unmounted. Run it under `nohup`, a terminal multiplexer, or a systemd unit if
you want it to persist.

### Example

```bash
mkdir -p /tmp/notebooks-stripped
fuse-stripped-notebooks ~/work/notebooks /tmp/notebooks-stripped &

ls /tmp/notebooks-stripped
cat /tmp/notebooks-stripped/analysis.ipynb   # JSON with outputs stripped

# In another shell, unmount when you're done:
fusermount3 -u /tmp/notebooks-stripped
```

To get the Python-script view instead:

```bash
fuse-stripped-notebooks --mode python-script ~/work/notebooks /tmp/notebooks-py
cat /tmp/notebooks-py/analysis.ipynb         # now a .py-style script
```

### Logging

The binary uses `env_logger`. Set `RUST_LOG` to see what's happening:

```bash
RUST_LOG=debug fuse-stripped-notebooks ~/work/notebooks /tmp/mnt
```

## Development

```bash
cargo check                              # fastest type-check loop
cargo build                              # debug build
cargo test                               # integration suite under tests/
cargo test -- --nocapture                # show stdout/stderr from the spawned binary
cargo test <name>                        # run a single test by substring match
cargo fmt                                # format the codebase in place
cargo clippy --all-targets -- -D warnings   # lint sources + tests; fail on any warning
```

Tests spawn the binary, mount it into a tempdir, and unmount on teardown. If a
run crashes and leaves a stale mount, clean it up with:

```bash
mount | grep fuse-stripped-notebooks
fusermount3 -u <path>
```

For ad-hoc exploration:

```bash
mkdir -p /tmp/src /tmp/mnt
cp tests/fixtures/sample.* /tmp/src/
RUST_LOG=debug cargo run -- /tmp/src /tmp/mnt &
ls /tmp/mnt
fusermount3 -u /tmp/mnt
```

## Limitations

- Read-only. Any write, create, delete, or rename through the mount returns
  `EROFS`.
- Linux only. macOS would need macFUSE plus a different build configuration;
  Windows is not supported.
- Symlinks in the source are exposed as symlinks; their targets are not
  rewritten, so a symlink pointing outside the source tree resolves to its
  real location.
