//! GL-free, pure-CPU STL -> PNG thumbnail renderer.
//!
//! Parses a binary or ASCII STL, auto-frames the model with a fixed iso-ish
//! camera, and renders it with the [`euc`] software rasterizer (z-buffer +
//! per-vertex-normal shading, directional + ambient light). No GPU, OpenGL,
//! wgpu, X11 or Mesa is involved — it runs identically on a headless server
//! and inside the desktop core.
//!
//! Anti-aliasing is done by supersampling: we render at `SSAA`x the requested
//! resolution and box-downsample. (euc 0.5.3 has no built-in MSAA; SSAA gives
//! equivalent edge quality on CPU and is trivially correct.)

use std::io::Cursor;

use anyhow::{Context, Result};
use euc::{
    buffer::Buffer2d,
    rasterizer::{BackfaceCullingDisabled, Triangles},
    DepthStrategy, Pipeline,
};
use image::{ImageEncoder, ImageError};
use vek::{Mat4, Vec3, Vec4};

/// Supersampling factor: render at SSAA x size, then box-downsample for AA.
const SSAA: u32 = 3;
/// Background color of the thumbnail (a dark neutral), 8-bit RGB.
const BG: [u8; 3] = [0x20, 0x22, 0x28];
/// Base material color (a warm light gray — fresh filament/plastic look).
const MATERIAL: Vec3<f32> = Vec3::new(0.78, 0.80, 0.84);
/// Ambient term so faces away from the key light aren't pure black.
const AMBIENT: f32 = 0.28;
/// World up. **+Z, the STL/3MF convention** — see `auto_frame`. Named rather than spelled inline
/// because the camera direction and the look-at both have to agree about it, and they did not.
const UP: Vec3<f32> = Vec3::new(0.0, 0.0, 1.0);

/// A single triangle vertex fed to the rasterizer: world position + normal.
#[derive(Clone)]
struct Vert {
    pos: Vec3<f32>,
    normal: Vec3<f32>,
}

/// euc render pipeline: MVP transform in `vert`, two-sided Lambert in `frag`.
struct ThumbPipeline {
    mvp: Mat4<f32>,
    /// Direction *towards* the key light, world space (normalized).
    light_dir: Vec3<f32>,
}

impl Pipeline for ThumbPipeline {
    type Vertex = Vert;
    // Interpolated per-fragment data: the world-space normal. `Vec3<f32>` implements
    // euc's `Interpolate`, so the rasterizer blends it barycentrically across each
    // triangle -> smooth (Gouraud-style) shading.
    type VsOut = Vec3<f32>;
    type Pixel = u32;

    #[inline]
    fn get_depth_strategy(&self) -> DepthStrategy {
        DepthStrategy::IfLessWrite
    }

    #[inline]
    fn vert(&self, v: &Self::Vertex) -> ([f32; 4], Self::VsOut) {
        let clip = self.mvp * Vec4::new(v.pos.x, v.pos.y, v.pos.z, 1.0);
        ([clip.x, clip.y, clip.z, clip.w], v.normal)
    }

    #[inline]
    fn frag(&self, normal: &Self::VsOut) -> Self::Pixel {
        // Two-sided Lambert: |n . l| so back-facing geometry is lit too.
        let n = normal.try_normalized().unwrap_or(Vec3::unit_z());
        let diffuse = n.dot(self.light_dir).abs();
        let intensity = (AMBIENT + (1.0 - AMBIENT) * diffuse).min(1.0);
        let c = MATERIAL * intensity;
        let r = (c.x.clamp(0.0, 1.0) * 255.0) as u32;
        let g = (c.y.clamp(0.0, 1.0) * 255.0) as u32;
        let b = (c.z.clamp(0.0, 1.0) * 255.0) as u32;
        // Packed 0x00RRGGBB; unpacked when encoding the PNG.
        (r << 16) | (g << 8) | b
    }
}

/// Render an STL mesh (binary or ASCII, auto-detected) to a PNG thumbnail of
/// `w` x `h` pixels. Returns the encoded PNG bytes.
///
/// Empty meshes yield a valid blank (background-filled) PNG. Unparseable input
/// returns an `Err` — it never panics.
pub fn render_stl(stl: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let w = w.max(1);
    let h = h.max(1);

    let mut cursor = Cursor::new(stl);
    let mesh =
        stl_io::read_stl(&mut cursor).context("failed to parse STL (not binary or ASCII)")?;

    let verts = build_vertices(&mesh);

    // Render at the supersampled resolution.
    let sw = (w * SSAA) as usize;
    let sh = (h * SSAA) as usize;
    let bg_packed = ((BG[0] as u32) << 16) | ((BG[1] as u32) << 8) | (BG[2] as u32);
    let mut color = Buffer2d::new([sw, sh], bg_packed);
    let mut depth = Buffer2d::new([sw, sh], 1.0f32);

    if !verts.is_empty() {
        let mvp = auto_frame(&verts, sw as u32, sh as u32);
        // Key light from the upper-front-right — classic product-shot direction. Z-UP LIKE THE
        // CAMERA: the "upper" component is the Z one. Shading is two-sided (`|n . l|`), so this is
        // cosmetic rather than correctness — but a light left orbiting the old axis would relight
        // every model sideways and quietly undo half of what the camera fix bought.
        let light_dir = Vec3::new(0.5, 0.6, 0.7).normalized();
        ThumbPipeline { mvp, light_dir }.draw::<Triangles<_, BackfaceCullingDisabled>, _>(
            &verts,
            &mut color,
            Some(&mut depth),
        );
    }

    let down = downsample(color.as_ref(), sw, sh, SSAA);
    encode_png(&down, w, h)
}

/// Flatten the indexed mesh into a flat triangle-vertex list, attaching a usable
/// normal to each vertex. Uses the STL face normal when present and non-zero;
/// otherwise recomputes it from the triangle geometry (cross product).
fn build_vertices(mesh: &stl_io::IndexedMesh) -> Vec<Vert> {
    let mut out = Vec::with_capacity(mesh.faces.len() * 3);
    for face in &mesh.faces {
        let p = |i: usize| {
            let v = mesh.vertices[face.vertices[i]];
            Vec3::new(v[0], v[1], v[2])
        };
        let (a, b, c) = (p(0), p(1), p(2));

        let stl_n = Vec3::new(face.normal[0], face.normal[1], face.normal[2]);
        let normal = if stl_n.magnitude_squared() > 1e-12 {
            stl_n.normalized()
        } else {
            // Recompute from geometry; degenerate (zero-area) tris fall back to +Z.
            (b - a)
                .cross(c - a)
                .try_normalized()
                .unwrap_or(Vec3::unit_z())
        };

        for pos in [a, b, c] {
            out.push(Vert { pos, normal });
        }
    }
    out
}

/// Build a model-view-projection matrix that frames the whole model with a fixed
/// iso-ish camera (~30 deg pitch, ~45 deg yaw) and ~10% margin.
///
/// **THE WORLD IS Z-UP, BECAUSE STL IS.** The format itself declares no orientation, but every
/// producer and consumer in 3D printing and CAD — Fusion, SolidWorks, Onshape, FreeCAD, and every
/// slicer, which puts the build plate at Z=0 — treats +Z as up. So does the 3MF core spec, and
/// `ThreeMf#mesh_to_stl` feeds this same function.
///
/// This used to orbit Y and pass `Vec3::unit_y()` as the up vector, i.e. render a Z-up model under
/// a Y-up camera: every thumbnail in the library came out **tipped onto its side**, consistently
/// enough to read as a house style rather than a bug. Nothing errored and every existing test
/// stayed green, because they all ask "is the frame non-blank?" — a question a model lying down
/// answers exactly as well as one standing up. Only a POSITIONAL assertion can see this, which is
/// what `tall_model_renders_taller_than_wide` is.
fn auto_frame(verts: &[Vert], w: u32, h: u32) -> Mat4<f32> {
    let mut min = Vec3::broadcast(f32::INFINITY);
    let mut max = Vec3::broadcast(f32::NEG_INFINITY);
    for v in verts {
        min = Vec3::partial_min(min, v.pos);
        max = Vec3::partial_max(max, v.pos);
    }
    let center = (min + max) * 0.5;
    let radius = ((max - min).magnitude() * 0.5).max(1e-6);

    // Iso-ish camera direction: 45 deg of yaw about the UP axis (Z), ~30 deg above the horizon.
    // The pitch therefore belongs on Z and the yaw sweeps X/Y — the mirror image of the Y-up
    // spelling this replaces.
    let yaw = 45f32.to_radians();
    let pitch = 30f32.to_radians();
    let dir = Vec3::new(
        yaw.cos() * pitch.cos(),
        yaw.sin() * pitch.cos(),
        pitch.sin(),
    )
    .normalized();

    let fov = 45f32.to_radians();
    // Distance so the bounding sphere fits in the FOV, plus a 10% margin.
    let distance = radius / (fov * 0.5).sin() * 1.10;
    let eye = center + dir * distance;

    let proj = Mat4::perspective_fov_rh_no(fov, w as f32, h as f32, 0.01, distance + radius * 4.0);
    let view = Mat4::look_at_rh(eye, center, UP);
    proj * view
}

/// Box-downsample a packed-`0x00RRGGBB` buffer of `sw`x`sh` by `factor`,
/// returning interleaved RGBA8 of `(sw/factor)`x`(sh/factor)`.
fn downsample(src: &[u32], sw: usize, sh: usize, factor: u32) -> Vec<u8> {
    let f = factor as usize;
    let dw = sw / f;
    let dh = sh / f;
    let mut out = Vec::with_capacity(dw * dh * 4);
    let n = (f * f) as u32;
    for dy in 0..dh {
        for dx in 0..dw {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for sy in 0..f {
                for sx in 0..f {
                    let px = src[(dy * f + sy) * sw + (dx * f + sx)];
                    r += (px >> 16) & 0xff;
                    g += (px >> 8) & 0xff;
                    b += px & 0xff;
                }
            }
            out.push((r / n) as u8);
            out.push((g / n) as u8);
            out.push((b / n) as u8);
            out.push(0xff);
        }
    }
    out
}

/// Encode interleaved RGBA8 pixels to PNG.
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e: ImageError| anyhow::anyhow!(e))
        .context("failed to encode PNG")?;
    Ok(out)
}
