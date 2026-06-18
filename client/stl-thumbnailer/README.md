# stl-thumbnailer

GL-free, pure-CPU STL → PNG thumbnail renderer. A software rasterizer (no GPU,
OpenGL, wgpu, Vulkan/Metal, X11/EGL/Mesa, or headless browser) so it runs
identically:

- **server-side** — baked into the Rails Docker image (no Mesa/GPU needed), and
- **desktop** — inside the Tauri Rust core.

## Use

Library:

```rust
let png: Vec<u8> = stl_thumbnailer::render_stl(&stl_bytes, 256, 256)?;
```

CLI:

```sh
stl-thumbnailer <in.stl> <out.png> [w] [h]   # default 256x256, non-zero exit on error
```

## How it renders

Parse STL (binary + ASCII, auto-detected) → compute AABB → auto-frame with a
fixed iso-ish camera (45° yaw / 30° pitch, ~10% margin) → rasterize with
[`euc`] (z-buffer, two-sided Lambert, per-vertex-normal Gouraud shading,
directional key light + ambient) at 3× supersampling → box-downsample for
anti-aliasing → encode PNG with [`image`].

Robustness: empty/degenerate meshes produce a blank-but-valid PNG; missing or
zero STL normals are recomputed from triangle geometry; unparseable input
returns `Err` (never panics).

## Dependencies (all permissive, all GL-free)

| crate | version | license | role |
|---|---|---|---|
| `euc` | 0.5.3 (crates.io) | Apache-2.0 / MIT | CPU rasterizer |
| `vek` | 0.14 | MIT / Apache-2.0 | matrices + interpolated normals |
| `stl_io` | 0.8 | MIT | STL parse (binary + ASCII) |
| `image` | 0.25 (`png` only) | MIT / Apache-2.0 | PNG encode |
| `anyhow` | 1 | MIT / Apache-2.0 | error type |

The transitive tree contains **no** GPU/GL/windowing crate.

### Note on the `euc` version

The research pass suggested git-pinning `euc` to `master` (0.6.0). That was
tried first, but its rasterizer produced empty frames on the current `master`
commit (reproduced with a minimal mirror of the upstream `triangle.rs`
example). The crates.io **0.5.3** release renders correctly and is what we ship.
`vek` is pinned to **0.14** to match the version `euc` 0.5.3 implements its
`Interpolate` trait for (so our `Vec3<f32>` normal satisfies the `VsOut` bound).

`euc` 0.5.3 has no built-in MSAA, so anti-aliasing is done by 3× supersampling +
box downsample — equivalent edge quality on CPU and trivially correct.

## Integration notes

- **No system libraries.** Nothing to add to the Docker image beyond the Rust
  build; the resulting binary has no runtime GL/X11/Mesa dependency.
- **Stripped CLI binary ≈ 600 KB.** As a library, the added code is small (euc
  is `#![no_std]` + `alloc`; vek/stl_io/image are the bulk).
- **Fast for thumbnails.** ~8 ms for a 5 000-triangle mesh at 256×256 on this
  box; large meshes scale roughly linearly in triangle count (fine for
  thumbnails — no acceleration structure needed).
- **Deterministic + thread-safe.** `render_stl` is a pure function over its
  bytes; safe to call from a Rails background job or a Tauri command.
