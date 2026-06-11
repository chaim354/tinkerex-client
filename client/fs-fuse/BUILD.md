# Building / running the FUSE adapter

One filesystem implementation (`fs-fuse`) runs on every FUSE driver. The driver is
**installed by the user per OS** (keeps copyleft drivers out of our binaries):

- **Linux** — libfuse (`apt install libfuse-dev` / usually present).
- **macOS** — **FUSE-T** (https://www.fuse-t.org) — no kext, no macFUSE.
- **Windows** — **WinFsp** (https://winfsp.dev) — built via the `winfsp-rs` binding
  (not yet wired; `fuser` covers Linux + macOS).

## macOS (FUSE-T) build notes

`fuser` 0.14 probes pkg-config for `osxfuse`/`fuse` (the macFUSE names), but FUSE-T
ships `fuse-t.pc` / `fuse3.pc`. FUSE-T's `libfuse-t` *is* the macFUSE-compatible
(libfuse 2.x) API, so alias it:

```sh
brew install pkg-config            # if not present
mkdir -p /tmp/fuse-pc
for n in osxfuse fuse; do
  cat > /tmp/fuse-pc/$n.pc <<EOF
prefix=/usr/local
libdir=\${prefix}/lib
includedir=\${prefix}/include/fuse
Name: $n
Description: FUSE-T compat alias
Version: 2.9.7
Libs: -L\${libdir} -Wl,-rpath,\${libdir} -lfuse-t
Cflags: -I\${includedir}
EOF
done

PKG_CONFIG_PATH=/tmp/fuse-pc:/usr/local/lib/pkgconfig cargo build
```

The runtime rpath is handled by `.cargo/config.toml`. Then:

```sh
./target/debug/maker-mount ~/makermnt          # Ctrl-C to unmount
```

mkdir / write / read / ls through `~/makermnt` are served by the local SQLite tree
+ content-addressed chunk store; files persist across remounts.

> TODO: replace the manual pkg-config alias with a `build.rs` that detects FUSE-T,
> and add the `winfsp-rs` adapter for Windows.
