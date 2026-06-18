//! Integration tests for the STL -> PNG thumbnailer.
//!
//! Quality gate: the render must be a *real* shaded model, not a blank/noisy frame.

use image::GenericImageView;
use stl_thumbnailer::render_stl;

const TETRA: &[u8] = include_bytes!("fixtures/tetra.stl");
const CUBE_ASCII: &[u8] = include_bytes!("fixtures/cube_ascii.stl");

/// Decode PNG bytes; assert dimensions; return the RGBA pixels as a flat Vec.
fn decode(png: &[u8], w: u32, h: u32) -> Vec<[u8; 4]> {
    let img = image::load_from_memory(png).expect("output must be a decodable PNG");
    assert_eq!(img.dimensions(), (w, h), "PNG must be exactly w x h");
    img.to_rgba8().pixels().map(|p| p.0).collect()
}

/// Fraction of pixels that differ meaningfully from the most-common (background) color.
fn non_background_ratio(pixels: &[[u8; 4]]) -> f64 {
    use std::collections::HashMap;
    let mut counts: HashMap<[u8; 4], usize> = HashMap::new();
    for p in pixels {
        *counts.entry(*p).or_default() += 1;
    }
    let bg = *counts.iter().max_by_key(|(_, c)| **c).unwrap().0;
    let differ = pixels
        .iter()
        .filter(|p| {
            let dr = (p[0] as i32 - bg[0] as i32).abs();
            let dg = (p[1] as i32 - bg[1] as i32).abs();
            let db = (p[2] as i32 - bg[2] as i32).abs();
            dr + dg + db > 24
        })
        .count();
    differ as f64 / pixels.len() as f64
}

#[test]
fn renders_valid_png_of_exact_dimensions() {
    let png = render_stl(TETRA, 256, 256).expect("render should succeed");
    let _ = decode(&png, 256, 256);
}

#[test]
fn render_is_non_blank() {
    let png = render_stl(TETRA, 256, 256).expect("render should succeed");
    let pixels = decode(&png, 256, 256);
    let ratio = non_background_ratio(&pixels);
    assert!(
        ratio > 0.05,
        "model should occupy a meaningful fraction of the frame, got {ratio:.4}"
    );
}

#[test]
fn ascii_stl_path_works() {
    let png = render_stl(CUBE_ASCII, 200, 150).expect("ASCII STL should render");
    let pixels = decode(&png, 200, 150);
    let ratio = non_background_ratio(&pixels);
    assert!(
        ratio > 0.05,
        "ASCII cube should render a visible model, got {ratio:.4}"
    );
}

#[test]
fn non_square_dimensions_respected() {
    let png = render_stl(TETRA, 320, 180).expect("render should succeed");
    let _ = decode(&png, 320, 180);
}

#[test]
fn empty_mesh_does_not_panic_and_returns_valid_png() {
    // An empty (zero-triangle) binary STL: 80-byte header + u32(0) triangle count.
    let mut empty = vec![0u8; 80];
    empty.extend_from_slice(&0u32.to_le_bytes());
    let png = render_stl(&empty, 64, 64).expect("empty mesh should yield a blank-but-valid PNG");
    let pixels = decode(&png, 64, 64);
    // Blank: essentially all background, no crash.
    let ratio = non_background_ratio(&pixels);
    assert!(ratio < 0.01, "empty mesh should be ~blank, got {ratio:.4}");
}

#[test]
fn garbage_input_errors_not_panics() {
    let res = render_stl(b"not an stl file at all", 64, 64);
    assert!(res.is_err(), "garbage input should Err, not panic");
}
