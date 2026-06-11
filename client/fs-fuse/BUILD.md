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

### Runtime gotcha — use NFS-based FUSE-T, not the FSKit build (macOS 26+)

FUSE-T ≥1.1 ships an experimental **FSKit** backend and defaults to it on macOS 26+.
`libfuse-t` loads and `mount2`/`spawn_mount2` is called, but the mount **silently never
establishes** (no error): `maker-mount` blocks with nothing in `mount`, and the daemon's
`spawn_mount2` session ends immediately so the app shows "unmounted." FSKit modules are
third-party extensions requiring approval in *System Settings → General → Login Items &
Extensions → File System Extensions*, and that toggle is frequently greyed/broken on
current macOS. Forcing the NFS backend (`-o backend=nfs`) didn't help either — the
`go-nfsv4` server doesn't spawn.

**Fix:** install the **standard NFS-based FUSE-T** (the plain `fuse-t` pkg from the
[releases](https://github.com/macos-fuse-t/fuse-t/releases), *not* the `*-fskit` one),
which mounts via a local NFS loopback with no extension approval. Confirmed on macOS
26.3 that the FSKit build of 1.2.7 cannot mount even via the known-good `maker-mount`, so
this is FUSE-T-vs-macOS, not the adapter. On Linux, libfuse has none of this.

> TODO: replace the manual pkg-config alias with a `build.rs` that detects FUSE-T,
> and add the `winfsp-rs` adapter for Windows.
