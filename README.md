# fuse-stripped-notebooks

A read-only FUSE filesystem that mirrors a source directory and transforms
Jupyter notebooks (`.ipynb`) on the fly. Everything else passes through
unchanged.

## Why

Notebooks (`.ipynb` files) mix code and outputs, which is bad for tools like `grep`.

## Example

Let's say I want to use `grep` to search in code, but the following command returns many results from output cells
```shell
grep -rni bartosz.marcinkowski notebooks
```

With `fuse-stripped-notebooks`, I run
```shell
fuse-stripped-notebooks --source notebooks --mountpoint stripped --mode python-script --mkdir &
```
and then
```shell
grep -rni bartosz.marcinkowski stripped
```
gives me clean results.

The process keeps running until it's terminated by a signal or calling `umount` on the mountpoint.

## Usage

```
$ fuse-stripped-notebooks --help
Run FUSE stripping notebooks until stopped by signal or external umount

Usage: fuse-stripped-notebooks [OPTIONS] --source <SOURCE> --mountpoint <MOUNTPOINT>

Options:
      --mode <MODE>
          How .ipynb files are transformed.
          
          python-script: Code and Markdown input cell content divided by comments with cell numbers
          
          strip-outputs: Notebook content without output cells and execution count
          
          [default: python-script]
          [possible values: strip-outputs, python-script]

      --source <SOURCE>
          Directory containing source notebooks

      --mountpoint <MOUNTPOINT>
          Directory to mount the virtual filesystem at

      --mkdir
          Create the mountpoint directory if it does not exist

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

```

## Installation

```
cargo install fuse-stripped-notebooks
```

### System requirements - Ubuntu

Install FUSE
```bash
sudo apt-get update
sudo apt-get install -y fuse3 build-essential
```

Install `cargo` with [rustup](https://rustup.rs/)
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
