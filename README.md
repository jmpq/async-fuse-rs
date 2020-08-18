# Rust FUSE - Filesystem in Userspace

![Crates.io](https://img.shields.io/crates/l/fuse)
[![Crates.io](https://img.shields.io/crates/v/fuse)](https://crates.io/crates/fuse)

## About

**async-fuse-rs** is a async [Rust] library crate based on the rust-rs crate for easy implementation of [FUSE filesystems][libfuse] in userspace.


## Documentation

[Crate documentation][documentation]

## Details

A working FUSE filesystem consists of three parts:

1. The **kernel driver** that registers as a filesystem and forwards operations into a communication channel to a userspace process that handles them.
1. The **userspace library** (libfuse) that helps the userspace process to establish and run communication with the kernel driver.
1. The **userspace implementation** that actually processes the filesystem operations.

The kernel driver is provided by the FUSE project, the userspace implementation needs to be provided by the developer. fuse-rs provides a replacement for the libfuse userspace library between these two. This way, a developer can fully take advantage of the Rust type interface and runtime features when building a FUSE filesystem in Rust.

Except for a single setup (mount) function call and a final teardown (unmount) function call to libfuse, everything runs in Rust.

## Dependencies

FUSE must be installed to build or run programs that use fuse-rs (i.e. kernel driver and libraries. Some platforms may also require userland utils like `fusermount`). A default installation of FUSE is usually sufficient.

To build fuse-rs or any program that depends on it, `pkg-config` needs to be installed as well.

### Linux

[FUSE for Linux][libfuse] is available in most Linux distributions and usually called `fuse`. To install on a Debian based system:

```sh
sudo apt-get install fuse
```

Install on CentOS:

```sh
sudo yum install fuse
```

To build, FUSE libraries and headers are required. The package is usually called `libfuse-dev` or `fuse-devel`. Also `pkg-config` is required for locating libraries and headers.

```sh
sudo apt-get install libfuse-dev pkg-config
```

```sh
sudo yum install fuse-devel pkgconfig
```

### macOS

Installer packages can be downloaded from the [FUSE for macOS homepage][FUSE for macOS].

To install using [Homebrew]:

```sh
brew cask install osxfuse
```

To install `pkg-config` (required for building only):

```sh
brew install pkg-config
```

### FreeBSD

Install packages `fusefs-libs` and `pkgconf`.

```sh
pkg install fusefs-libs pkgconf
```

## Usage

Put this in your `Cargo.toml`:

```toml
[dependencies]
async-fuse = { git = "https://github.com/jmpq/async-fuse-rs" }
```

To create a new filesystem, implement the trait `fuse::Filesystem`. See the documentation for details or the `examples` directory for some basic examples.
