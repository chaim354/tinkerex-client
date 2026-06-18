//! GL-free, pure-CPU STL -> PNG thumbnail renderer.
//!
//! TDD stub. Real implementation follows the failing tests.

use anyhow::Result;

/// Render an STL mesh (binary or ASCII) to a PNG thumbnail of `w` x `h` pixels.
///
/// Returns the encoded PNG bytes.
pub fn render_stl(_stl: &[u8], _w: u32, _h: u32) -> Result<Vec<u8>> {
    anyhow::bail!("not implemented")
}
