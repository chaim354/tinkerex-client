//! CLI: `stl-thumbnailer <in.stl> <out.png> [w] [h]` (default 256x256).
//! Exits non-zero on any error.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stl-thumbnailer: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: stl-thumbnailer <in.stl> <out.png> [w] [h]"))?;
    let output = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: stl-thumbnailer <in.stl> <out.png> [w] [h]"))?;
    let w: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(256);
    let h: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(256);

    let stl = std::fs::read(&input).map_err(|e| anyhow::anyhow!("reading {input}: {e}"))?;
    let png = stl_thumbnailer::render_stl(&stl, w, h)?;
    std::fs::write(&output, &png).map_err(|e| anyhow::anyhow!("writing {output}: {e}"))?;
    eprintln!("wrote {output} ({w}x{h}, {} bytes)", png.len());
    Ok(())
}
