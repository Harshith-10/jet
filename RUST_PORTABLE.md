Yes, there is a public API that provides exactly this information. It is maintained by the Rust infrastructure team and is the exact same source of truth that `rustup` uses under the hood.

However, there is a slight catch: the manifest is provided in **TOML** format rather than JSON.

Here is everything you need to reliably fetch the latest standalone installers.

### The API Endpoints

The Rust release manifests are hosted statically. Depending on the release channel you want to track, you can fetch one of the following:

* **Stable:** `https://static.rust-lang.org/dist/channel-rust-stable.toml`
* **Beta:** `https://static.rust-lang.org/dist/channel-rust-beta.toml`
* **Nightly:** `https://static.rust-lang.org/dist/channel-rust-nightly.toml`

### A Quick Note on `rustc` vs. `rust-std`

You mentioned that for an online judge, just getting `rustc` should suffice. It's a logical assumption since you likely don't need `cargo` to compile single-file submissions.

However, if you download *only* the `rustc` component, you won't be able to compile anything—not even a simple "Hello, World!". The compiler requires the Rust standard library (`std`) for the specific target architecture to actually link and build the executable.

To get a working setup for an online judge, you have two routes:

1. **The Unified Installer (Recommended):** Look for the `rust` package in the manifest. It bundles `rustc`, `rust-std`, and `cargo` in a single standalone tarball with a handy `install.sh` script.
2. **The Minimal Approach:** Look for both the `rustc` and `rust-std` packages in the manifest, download them separately, and unpack them into the same sysroot directory.

### Parsing the Manifest

If you decide to go with the unified standalone installer, you can parse the TOML file to find the `.tar.xz` (or `.tar.gz`) URLs and their corresponding checksum hashes.

Here are the exact TOML paths you will need for your requested architectures:

* **Linux x86_64:** * URL: `pkg.rust.target.x86_64-unknown-linux-gnu.xz_url`
* Hash: `pkg.rust.target.x86_64-unknown-linux-gnu.xz_hash`


* **Linux aarch64 (ARM64):** * URL: `pkg.rust.target.aarch64-unknown-linux-gnu.xz_url`
* Hash: `pkg.rust.target.aarch64-unknown-linux-gnu.xz_hash`



*Note: If you want to use statically linked musl libc instead of glibc for your judge environment, swap `unknown-linux-gnu` with `unknown-linux-musl`.*

Here is a simplified look at how the data is structured inside the TOML file:

```toml
manifest-version = "2"
date = "2024-02-08"

[pkg.rust]
version = "1.76.0 (07dca489a 2024-02-04)"

  [pkg.rust.target.x86_64-unknown-linux-gnu]
  available = true
  url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-x86_64-unknown-linux-gnu.tar.gz"
  hash = "..."
  xz_url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-x86_64-unknown-linux-gnu.tar.xz"
  xz_hash = "..."

```

---

For an online judge, you want a minimal, isolated environment where you can just drop a folder in and run `./bin/rustc` without polluting the host system or running installation scripts.

Here is the reality of how Rust packages are distributed: **There isn't a single pre-built "portable" tarball you can just extract and run.** The unified `rust` tarball I mentioned earlier is actually just a wrapper containing separate components (`rustc`, `rust-std`, `cargo`) and an `install.sh` script. If you just extract it, `rustc` won't know where to find the standard library because they sit in separate subfolders.

However, **you can easily build your own portable directory.** Rust compilers are designed to be fully relocatable as long as they are placed inside a standard "sysroot" directory structure. `rustup` actually creates this exact structure under the hood.

Here is how you achieve a truly portable, extract-and-run Rust setup.

### The "Portable Sysroot" Approach

To make a portable compiler, you need to download the individual `rustc` and `rust-std` components and merge them into a single directory.

Because `rustc` uses relative paths (`$ORIGIN/../lib`) to find its dependencies, merging them makes the compiler instantly portable.

#### 1. The TOML Keys You Need

Instead of looking at the `[pkg.rust]` section, you will want to extract the URLs for the standalone compiler and standard library packages:

**For `rustc`:**

* `pkg.rustc.target.x86_64-unknown-linux-gnu.xz_url`
* `pkg.rustc.target.aarch64-unknown-linux-gnu.xz_url`

**For `rust-std`:**

* `pkg.rust-std.target.x86_64-unknown-linux-gnu.xz_url`
* `pkg.rust-std.target.aarch64-unknown-linux-gnu.xz_url`

#### 2. Merging the Components

When you download and extract these `.tar.xz` files, you will see a structure like this:

* `rustc-1.xx.x-x86_64.../rustc/bin/`
* `rustc-1.xx.x-x86_64.../rustc/lib/`
* `rust-std-1.xx.x-x86_64.../rust-std-x86_64.../lib/`

To make it portable, create a folder (e.g., `portable-rust/`) and copy the contents of the inner `rustc/` and `rust-std.../` directories into it so that their `lib/` directories merge.

#### 3. The Final Portable Structure

Once merged, your folder will look like this:

```text
portable-rust/
├── bin/
│   ├── rustc      <-- Run this directly!
│   └── rustdoc
└── lib/
    ├── librustc_driver.so
    └── rustlib/   <-- This is where the standard library lives

```

You can now move `portable-rust/` anywhere on your system. Calling `/path/to/portable-rust/bin/rustc main.rs` will successfully compile your code without any global installation or environment variables required.
