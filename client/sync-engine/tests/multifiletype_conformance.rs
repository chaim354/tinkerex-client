//! **Multi-filetype conformance** — round-trip a diverse maker-file corpus through the FULL
//! data plane (push → server changelog → chunk store → pull → hydrate) and assert each file
//! comes back **byte-for-byte identical**, that the content-defined chunker produces the
//! expected shape (multi-chunk on big files), and that **dedup** holds across an edit (a small
//! change to a large file re-uploads only the affected chunks, not the whole file).
//!
//! ## Why this exists (beyond integration.rs)
//!
//! `integration.rs` proves the round-trip works for a couple of synthetic byte streams. This
//! suite stresses the chunker + transport against the *actual byte shapes* the product ships
//! (§1 of CLAUDE.md): STL (ascii + binary), 3MF (a zip), STEP/IGES (ascii CAD), OBJ,
//! LightBurn `.lbrn2` (xml), PNG/JPG (binary, incompressible-ish), plain text/markdown,
//! G-code, and large multi-chunk binaries — incompressible random AND highly-repetitive (the
//! two extremes for FastCDC: random → many distinct chunks; repetitive → heavy intra-file
//! dedup). The files are synthetic (no real CAD semantics) but carry realistic byte patterns:
//! correct magic bytes, headers, structure, and entropy profile, which is all the
//! content-addressed data plane sees.
//!
//! Runs the corpus **sequentially** (the matrix: one assertion per filetype) AND
//! **concurrently** (every filetype pushed by its own thread at once → the data plane under a
//! mixed-type write storm), both byte-for-byte verified on a fresh reader device.
//!
//! Skips cleanly (prints a notice, passes) when no server is configured — same convention as
//! `integration.rs` / `two_device_e2e.rs`. CI boots `STORAGE_BACKEND=memory` Rails and sets
//! `SYNC_IT_SERVER_URL` (the nightly `multifiletype-conformance` job).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

/// `Some(url)` if a test server is configured, else early-return with a skip notice.
macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run multi-filetype conformance)");
                return;
            }
        }
    };
}

/// Production-shaped chunk config (NOT the TINY config the other suites use). FastCDC defaults
/// are ~16K/64K/256K; we use a smaller-but-realistic window so a few-hundred-KB file is a
/// genuine many-chunk file (real dedup coverage) without needing tens of MB for every type.
const CHUNK: ChunkConfig = ChunkConfig {
    min: 2 * 1024,
    avg: 8 * 1024,
    max: 32 * 1024,
};

fn uniq() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    )
}

/// A device: isolated local tree + its own chunk cache (so a reader genuinely hydrates from
/// the shared "cloud", exercising the data plane — same rule as integration.rs's `dev()`).
fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("mft-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), CHUNK).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 {
        Vec::new()
    } else {
        p.read(ino, 0, size as u32).unwrap()
    }
}

fn two_devices(url: &str) -> (ServerClient, ServerClient, SharedDirIO) {
    let mut a = ServerClient::new(url);
    a.enroll(
        &format!("mft-{}@example.com", uniq()),
        "correct horse battery staple",
        "device-A",
    )
    .expect("register A");
    let dev_b = a.create_device("device-B").expect("create device B");
    let b = ServerClient::with_token(url, dev_b.device_token);
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("mft-cloud-{}", uniq()))).unwrap();
    (a, b, cloud)
}

// ----------------------------------------------------------------------------------------
// Deterministic byte generators (no rand dep — a seeded LCG, like the other suites).
// ----------------------------------------------------------------------------------------

struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 1
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }
    fn fill(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
    fn f32(&mut self) -> f32 {
        // A finite float in a plausible coordinate range.
        (self.next_u64() as f32 / u64::MAX as f32) * 200.0 - 100.0
    }
}

/// A minimal but structurally-correct **binary STL**: 80-byte header, u32 triangle count,
/// then 50 bytes per triangle (12 f32 + u16 attr). Incompressible-ish floats → real chunks.
fn stl_binary(seed: u64, triangles: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut out = Vec::with_capacity(84 + triangles * 50);
    out.extend_from_slice(b"binary STL  synthetic conformance corpus");
    out.resize(80, 0); // 80-byte header
    out.extend_from_slice(&(triangles as u32).to_le_bytes());
    for _ in 0..triangles {
        for _ in 0..12 {
            out.extend_from_slice(&r.f32().to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // attribute byte count
    }
    out
}

/// An **ASCII STL**: the `solid ... facet ... endsolid` text grammar. Text → different chunker
/// behavior than binary; this is the human-readable 3D format.
fn stl_ascii(seed: u64, facets: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::from("solid conformance\n");
    for _ in 0..facets {
        s.push_str(&format!(
            "  facet normal {} {} {}\n",
            r.f32(),
            r.f32(),
            r.f32()
        ));
        s.push_str("    outer loop\n");
        for _ in 0..3 {
            s.push_str(&format!(
                "      vertex {} {} {}\n",
                r.f32(),
                r.f32(),
                r.f32()
            ));
        }
        s.push_str("    endloop\n  endfacet\n");
    }
    s.push_str("endsolid conformance\n");
    s.into_bytes()
}

/// A **3MF** — a real ZIP (3MF is OPC/zip). We build a valid zip container by hand (stored,
/// no compression) with a `[Content_Types].xml` + a `3D/3dmodel.model` part. The zip magic +
/// central directory make it a faithful binary-container shape for the chunker/transport.
fn threemf(seed: u64) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    // Two parts: content-types and the model XML (with some bulk vertex data for size).
    let content_types = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/></Types>"#.to_vec();
    let mut model = String::from(
        r#"<?xml version="1.0"?><model unit="millimeter"><resources><object id="1" type="model"><mesh><vertices>"#,
    );
    for _ in 0..2000 {
        model.push_str(&format!(
            r#"<vertex x="{}" y="{}" z="{}"/>"#,
            r.f32(),
            r.f32(),
            r.f32()
        ));
    }
    model.push_str("</vertices></mesh></object></resources></model>");
    let parts: [(&str, Vec<u8>); 2] = [
        ("[Content_Types].xml", content_types),
        ("3D/3dmodel.model", model.into_bytes()),
    ];
    build_zip(&parts)
}

/// Build a minimal valid ZIP archive (stored entries, no compression). Real PK magic + local
/// headers + central directory + EOCD — what a 3MF reader (and our transport) actually sees.
fn build_zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offsets = Vec::new();
    for (name, data) in parts {
        let offset = out.len() as u32;
        offsets.push(offset);
        let crc = crc32(data);
        out.extend_from_slice(b"PK\x03\x04"); // local file header
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // ver, flags, method=store, time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncomp size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    for (i, (name, data)) in parts.iter().enumerate() {
        let crc = crc32(data);
        central.extend_from_slice(b"PK\x01\x02"); // central dir header
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // versions/flags/method/time/date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // extra/comment/disk/attrs
        central.extend_from_slice(&offsets[i].to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06"); // EOCD
    out.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
    out.extend_from_slice(&(parts.len() as u16).to_le_bytes());
    out.extend_from_slice(&(parts.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// A **STEP** (ISO 10303-21) ASCII CAD file: the `ISO-10303-21; HEADER; DATA; ... ENDSEC;`
/// grammar with many `#N=` entity lines.
fn step(seed: u64, entities: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::from(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('synthetic'),'2;1');\nFILE_NAME('c.step','2026-06-15',(''),(''),'','','');\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n",
    );
    for i in 1..=entities {
        s.push_str(&format!(
            "#{i}=CARTESIAN_POINT('',({:.6},{:.6},{:.6}));\n",
            r.f32(),
            r.f32(),
            r.f32()
        ));
    }
    s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
    s.into_bytes()
}

/// An **IGES** ASCII CAD file: 80-column fixed-width records with the S/G/D/P/T section codes.
fn iges(seed: u64, records: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::new();
    let line = |body: String, sect: char, n: usize| {
        let mut l = body;
        l.truncate(72);
        format!("{l:<72}{sect}{n:>7}\n")
    };
    s.push_str(&line("Synthetic IGES conformance corpus.".into(), 'S', 1));
    s.push_str(&line("1H,,1H;,4HSLF,".into(), 'G', 1));
    for i in 1..=records {
        s.push_str(&line(
            format!("116,{:.4},{:.4},{:.4},0,0,0;", r.f32(), r.f32(), r.f32()),
            'P',
            i,
        ));
    }
    s.push_str(&line(
        format!("S{:>7}G{:>7}D{:>7}P{:>7}", 1, 1, 0, records),
        'T',
        1,
    ));
    s.into_bytes()
}

/// A **Wavefront OBJ** mesh: `v`/`vn`/`f` lines.
fn obj(seed: u64, verts: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::from("# synthetic OBJ\n");
    for _ in 0..verts {
        s.push_str(&format!("v {} {} {}\n", r.f32(), r.f32(), r.f32()));
    }
    for i in (1..verts).step_by(3) {
        s.push_str(&format!("f {} {} {}\n", i, i + 1, (i % verts) + 1));
    }
    s.into_bytes()
}

/// A **LightBurn `.lbrn2`** project: the XML grammar with `<Shape>` + base64-ish path data.
fn lbrn2(seed: u64, shapes: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::from(
        r#"<?xml version="1.0"?><LightBurnProject AppVersion="1.5" FormatVersion="1"><Thumbnail Source=""/>"#,
    );
    for _ in 0..shapes {
        s.push_str(&format!(
            r#"<Shape Type="Path" CutIndex="0"><XForm>1 0 0 1 {} {}</XForm><VertList>"#,
            r.f32(),
            r.f32()
        ));
        for _ in 0..40 {
            s.push_str(&format!("V{} {}c0 0 ", r.f32(), r.f32()));
        }
        s.push_str("</VertList></Shape>");
    }
    s.push_str("</LightBurnProject>");
    s.into_bytes()
}

/// A **PNG**: real 8-byte signature + IHDR + many IDAT chunks of (incompressible) random data
/// + IEND. CRCs are present per chunk so it's structurally a valid PNG container shape.
fn png(seed: u64, idat_chunks: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    };
    let ihdr = [0, 0, 4, 0, 0, 0, 3, 0, 8, 6, 0, 0, 0]; // 1024x768 RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    for _ in 0..idat_chunks {
        let data = r.fill(8192);
        chunk(&mut out, b"IDAT", &data);
    }
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A **JPEG**: SOI + APP0/JFIF + a body of (incompressible) random scan-like bytes + EOI. The
/// markers are real; the entropy-coded body is random (what a photo's bytes look like to CDC).
fn jpeg(seed: u64, body_kb: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut out = vec![0xFF, 0xD8]; // SOI
    out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0, len 16
    out.extend_from_slice(b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]); // SOS
                                                                                          // Entropy-coded body: random bytes, but never an EOI marker (0xFFD9) mid-stream.
    let mut body = r.fill(body_kb * 1024);
    for i in 0..body.len() {
        if body[i] == 0xFF {
            body[i] = 0xFE; // keep markers out of the body
        }
    }
    out.extend_from_slice(&body);
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
    out
}

/// **G-code**: the `G1 X.. Y.. Z.. E.. F..` line grammar a slicer emits — lots of small,
/// similar-but-distinct text lines.
fn gcode(seed: u64, moves: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let mut s = String::from("; synthetic gcode\nG28\nG90\nM104 S210\n");
    let mut e = 0.0f32;
    for _ in 0..moves {
        e += 0.01;
        s.push_str(&format!(
            "G1 X{:.3} Y{:.3} Z{:.3} E{:.4} F{}\n",
            r.f32().abs(),
            r.f32().abs(),
            (r.f32().abs()) % 50.0,
            e,
            1200 + (r.next_u64() % 600)
        ));
    }
    s.push_str("M104 S0\nM84\n");
    s.into_bytes()
}

/// Plain **markdown** (a project README / build log) — natural-language-ish text.
fn markdown(seed: u64, paras: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let words = [
        "print",
        "layer",
        "nozzle",
        "filament",
        "bracket",
        "tolerance",
        "infill",
        "support",
        "assembly",
        "calibrate",
        "extrude",
        "warp",
        "adhesion",
        "bed",
        "PETG",
        "PLA",
    ];
    let mut s = String::from("# Build Log\n\n");
    for p in 0..paras {
        s.push_str(&format!("## Step {}\n\n", p + 1));
        for _ in 0..6 {
            for _ in 0..12 {
                s.push_str(words[(r.next_u64() as usize) % words.len()]);
                s.push(' ');
            }
            s.push('\n');
        }
        s.push('\n');
    }
    s.into_bytes()
}

/// A **large incompressible binary** (random) — the FastCDC worst case: every byte distinct →
/// many content-defined chunks, no intra-file dedup. `mb` megabytes.
fn large_random(seed: u64, mb: usize) -> Vec<u8> {
    Lcg::new(seed).fill(mb * 1024 * 1024)
}

/// A **large highly-repetitive binary** — a block pattern repeated, the FastCDC best case:
/// the chunker should find the repeats and dedup heavily (few *distinct* chunks for a big
/// file). `mb` megabytes built from a repeated `block`.
fn large_repetitive(seed: u64, mb: usize) -> Vec<u8> {
    let mut r = Lcg::new(seed);
    let block = r.fill(64 * 1024); // a 64K motif
    let total = mb * 1024 * 1024;
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        out.extend_from_slice(&block);
    }
    out.truncate(total);
    out
}

// ----------------------------------------------------------------------------------------
// The corpus matrix.
// ----------------------------------------------------------------------------------------

struct CorpusFile {
    name: &'static str,
    bytes: Vec<u8>,
    /// Expect this file to span more than one content-defined chunk (the big/multi-chunk ones).
    expect_multichunk: bool,
}

/// Build the full corpus. Sizes are tuned so the "big" files are genuinely many-chunk under the
/// production-shaped CHUNK config, while the small grammar files stay realistic.
fn corpus(seed: u64) -> Vec<CorpusFile> {
    let f = |name, bytes, expect_multichunk| CorpusFile {
        name,
        bytes,
        expect_multichunk,
    };
    vec![
        f("cube.stl", stl_binary(seed + 1, 20_000), true), // ~1 MB binary STL
        f("bracket_ascii.stl", stl_ascii(seed + 2, 6_000), true), // ascii STL
        f("widget.3mf", threemf(seed + 3), true),          // zip container
        f("flange.step", step(seed + 4, 12_000), true),    // ascii STEP
        f("gear.iges", iges(seed + 5, 9_000), true),       // ascii IGES
        f("mesh.obj", obj(seed + 6, 30_000), true),        // OBJ
        f("engraving.lbrn2", lbrn2(seed + 7, 1_500), true), // LightBurn xml
        f("render.png", png(seed + 8, 40), true),          // PNG (incompressible IDAT)
        f("photo.jpg", jpeg(seed + 9, 300), true),         // JPEG (~300 KB random body)
        f("benchy.gcode", gcode(seed + 10, 25_000), true), // G-code
        f("README.md", markdown(seed + 11, 30), false),    // small markdown
        f(
            "notes.txt",
            b"a short plain-text note\nwith two lines\n".to_vec(),
            false,
        ), // tiny text
        f("empty.txt", Vec::new(), false),                 // zero-byte edge case
        f("scan_random.bin", large_random(seed + 12, 12), true), // 12 MB incompressible
        f(
            "backup_repetitive.bin",
            large_repetitive(seed + 13, 16),
            true,
        ), // 16 MB repetitive
    ]
}

// ----------------------------------------------------------------------------------------
// Helpers: push one file, pull on a fresh device, hydrate, assert byte-for-byte.
// ----------------------------------------------------------------------------------------

fn push_file(a: &SqliteProvider, ca: &ServerClient, cloud: &dyn ChunkIO, name: &str, body: &[u8]) {
    let f = a.create(ROOT_INO, name).unwrap();
    if !body.is_empty() {
        a.write(f.ino, 0, body).unwrap();
    }
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(a, ca, cloud).unwrap_or_else(|e| panic!("push {name}: {e}"));
}

/// Pull the whole tree on a fresh reader, hydrate `name`, return its bytes.
fn pull_and_read(cb: &ServerClient, cloud: &dyn ChunkIO, name: &str) -> Vec<u8> {
    let b = dev();
    SyncEngine::pull_snapshot(&b, cb).unwrap_or_else(|e| panic!("pull_snapshot: {e}"));
    let bf = b
        .lookup(ROOT_INO, name)
        .unwrap_or_else(|_| panic!("reader missing {name}"));
    if b.getattr(bf.ino).unwrap().size == 0 {
        return Vec::new();
    }
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    SyncEngine::hydrate(&b, cb, cloud, &node_id).unwrap_or_else(|e| panic!("hydrate {name}: {e}"));
    read_all(&b, bf.ino)
}

// ========================================================================================
// TEST 1: the sequential matrix — every filetype round-trips byte-for-byte + chunk shape.
// ========================================================================================

#[test]
fn every_filetype_roundtrips_byte_for_byte() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);
    let a = dev();

    let files = corpus(0xC0FFEE);
    let mut total_bytes = 0usize;
    let mut total_chunks = 0usize;
    let mut results: Vec<(String, usize, usize, bool)> = Vec::new(); // name, bytes, chunks, pass

    for cf in &files {
        push_file(&a, &ca, &cloud, cf.name, &cf.bytes);

        // Chunk-shape assertion (on the writer's manifest).
        let af = a.lookup(ROOT_INO, cf.name).unwrap();
        let n_chunks = if cf.bytes.is_empty() {
            0
        } else {
            let nid = a.node_id_for(af.ino).unwrap().unwrap();
            a.node_manifest_hashes(&nid).unwrap().len()
        };
        if cf.expect_multichunk {
            assert!(
                n_chunks > 1,
                "{}: expected multi-chunk under the prod chunk config, got {n_chunks} chunk(s) for {} bytes",
                cf.name,
                cf.bytes.len()
            );
        }

        // Byte-for-byte on a fresh reader device (full data plane: pull + hydrate).
        let got = pull_and_read(&cb, &cloud, cf.name);
        let pass = got == cf.bytes;
        assert!(
            pass,
            "{}: NOT byte-for-byte (len got {} vs want {})",
            cf.name,
            got.len(),
            cf.bytes.len()
        );

        total_bytes += cf.bytes.len();
        total_chunks += n_chunks;
        results.push((cf.name.to_string(), cf.bytes.len(), n_chunks, pass));
    }

    eprintln!(
        "\n==== MULTI-FILETYPE CONFORMANCE MATRIX ({} types) ====",
        results.len()
    );
    eprintln!(
        "{:<26} {:>12} {:>8}  {}",
        "file", "bytes", "chunks", "byte-for-byte"
    );
    for (name, bytes, chunks, pass) in &results {
        eprintln!(
            "{name:<26} {bytes:>12} {chunks:>8}  {}",
            if *pass { "PASS" } else { "FAIL" }
        );
    }
    eprintln!(
        "TOTAL: {} files, {} bytes ({:.1} MB), {} chunks — all byte-for-byte\n",
        results.len(),
        total_bytes,
        total_bytes as f64 / 1_048_576.0,
        total_chunks
    );
}

// ========================================================================================
// TEST 2: dedup on a small edit to a large file — only the touched chunks re-upload.
//
// Push a large file, then overwrite a small middle region and re-push. A `ChunkIO` decorator
// counts PUTs on the SECOND push: with content-defined chunking, an edit to one region must
// re-upload only the chunk(s) overlapping it (plus possibly its CDC-boundary neighbors), NOT
// the whole file. We assert the second push uploaded far fewer chunks than the file has, and
// that a fresh reader still gets the edited file byte-for-byte.
// ========================================================================================

/// A `ChunkIO` that records every `put` hash (the chunks actually uploaded), wrapping the real
/// `SharedDirIO`. Thread-safe so the concurrent test can share one.
struct CountingIO {
    inner: SharedDirIO,
    puts: Mutex<Vec<String>>,
    gets: Mutex<Vec<String>>,
}
impl CountingIO {
    fn new(inner: SharedDirIO) -> Self {
        Self {
            inner,
            puts: Mutex::new(Vec::new()),
            gets: Mutex::new(Vec::new()),
        }
    }
    fn put_count(&self) -> usize {
        self.puts.lock().unwrap().len()
    }
    fn reset(&self) {
        self.puts.lock().unwrap().clear();
        self.gets.lock().unwrap().clear();
    }
}
impl ChunkIO for CountingIO {
    fn put(
        &self,
        hash: &str,
        urls: &HashMap<String, String>,
        bytes: Vec<u8>,
    ) -> sync_engine::Result<()> {
        self.puts.lock().unwrap().push(hash.to_string());
        self.inner.put(hash, urls, bytes)
    }
    fn get(&self, hash: &str, url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        self.gets.lock().unwrap().push(hash.to_string());
        self.inner.get(hash, url)
    }
}

#[test]
fn small_edit_to_large_file_dedups_chunks() {
    let url = server_url!();
    let (ca, cb, _bare) = two_devices(&url);
    let cloud = CountingIO::new(
        SharedDirIO::new(std::env::temp_dir().join(format!("mft-dedup-{}", uniq()))).unwrap(),
    );

    let a = dev();
    // A 16 MB random file → many distinct chunks.
    let mut body = large_random(0xD3D, 16);
    let f = a.create(ROOT_INO, "scan.bin").unwrap();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();

    // First push: uploads the whole file (a fresh cloud). Count its chunks.
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("push v1");
    let nid = a.node_id_for(f.ino).unwrap().unwrap();
    let total_chunks = a.node_manifest_hashes(&nid).unwrap().len();
    let first_push_puts = cloud.put_count();
    assert!(
        total_chunks > 50,
        "need a genuinely many-chunk file; got {total_chunks}"
    );
    assert_eq!(
        first_push_puts, total_chunks,
        "first push uploads every chunk on a fresh cloud"
    );

    // Edit a small region in the MIDDLE (overwrite 1 KB at the 8 MB mark) and re-push.
    cloud.reset();
    let mid = 8 * 1024 * 1024;
    let patch: Vec<u8> = (0..1024u32).map(|i| (i ^ 0xA5) as u8).collect();
    body[mid..mid + patch.len()].copy_from_slice(&patch);
    a.write(f.ino, mid as u64, &patch).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("push v2");

    let second_push_puts = cloud.put_count();
    // The edit touches one CDC chunk (and at most its immediate boundary neighbors). Assert the
    // re-upload is a tiny fraction of the file — this is the whole point of block-delta sync.
    assert!(
        second_push_puts > 0 && second_push_puts <= 8,
        "small edit re-uploaded {second_push_puts} chunks (of {total_chunks}); expected a tiny handful (dedup)"
    );
    let dedup_ratio = 1.0 - (second_push_puts as f64 / total_chunks as f64);
    eprintln!(
        "\n==== DEDUP ON EDIT ====\nfile: {total_chunks} chunks; 1 KB middle edit re-uploaded {second_push_puts} chunks \
         ({:.1}% deduped)\n",
        dedup_ratio * 100.0
    );

    // The edited file is still byte-for-byte on a fresh reader.
    let got = pull_and_read(&cb, &cloud, "scan.bin");
    assert_eq!(
        got, body,
        "edited large file is byte-for-byte after dedup'd re-push"
    );
}

// ========================================================================================
// TEST 3: intra-file dedup — a highly-repetitive large file stores FEW distinct chunks.
// ========================================================================================

#[test]
fn repetitive_file_dedups_within_itself() {
    let url = server_url!();
    let (ca, cb, _bare) = two_devices(&url);
    let cloud = CountingIO::new(
        SharedDirIO::new(std::env::temp_dir().join(format!("mft-rep-{}", uniq()))).unwrap(),
    );

    let a = dev();
    // 16 MB built from a repeated 64 KB motif → the manifest references the same few chunks
    // many times, but the cloud stores each DISTINCT chunk once (content-addressed dedup).
    let body = large_repetitive(0xBEE5, 16);
    let f = a.create(ROOT_INO, "repetitive.bin").unwrap();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("push repetitive");

    let nid = a.node_id_for(f.ino).unwrap().unwrap();
    let manifest_refs = a.node_manifest_hashes(&nid).unwrap().len();
    let distinct_uploaded = {
        let puts = cloud.puts.lock().unwrap();
        puts.iter().collect::<std::collections::HashSet<_>>().len()
    };
    // The manifest spans the whole 16 MB (many refs), but the DISTINCT chunks uploaded is a
    // small fraction — the repeats deduped. (FastCDC won't perfectly align to the 64K motif,
    // but distinct chunks must be far below total refs.)
    assert!(
        distinct_uploaded * 4 < manifest_refs,
        "repetitive file deduped poorly: {distinct_uploaded} distinct of {manifest_refs} refs"
    );
    eprintln!(
        "\n==== INTRA-FILE DEDUP ====\n16 MB repetitive file: {manifest_refs} manifest refs, only {distinct_uploaded} DISTINCT chunks uploaded \
         ({:.1}x dedup)\n",
        manifest_refs as f64 / distinct_uploaded.max(1) as f64
    );

    let got = pull_and_read(&cb, &cloud, "repetitive.bin");
    assert_eq!(
        got, body,
        "repetitive file byte-for-byte after intra-file dedup"
    );
}

// ========================================================================================
// TEST 4: CONCURRENT multi-filetype storm — every filetype pushed by its own thread at once,
// all into one library, then every one verified byte-for-byte on fresh readers.
// ========================================================================================

#[test]
fn concurrent_multifiletype_storm_no_corruption() {
    let url = server_url!();
    let (ca, cb, cloud_bare) = two_devices(&url);
    let cloud = std::sync::Arc::new(cloud_bare);

    let files = std::sync::Arc::new(corpus(0x5EED));
    let url_owned = url.clone();
    let token = ca.token().unwrap().to_string();

    // One writer thread per filetype, each its own device (own provider + own client), all
    // committing to the same library concurrently. A torn manifest / refcount race / changelog
    // interleaving bug would show up as a missing-or-corrupt file on read-back.
    let mut handles = Vec::new();
    for idx in 0..files.len() {
        let files = files.clone();
        let cloud = cloud.clone();
        let url = url_owned.clone();
        let token = token.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            let cf = &files[idx];
            let f = p.create(ROOT_INO, cf.name).unwrap();
            if !cf.bytes.is_empty() {
                p.write(f.ino, 0, &cf.bytes).unwrap();
            }
            p.flush(f.ino).unwrap();
            // Retry the push a few times — concurrent commits can transiently race on the
            // server's seq allocation; the engine is idempotent so a retry is safe.
            let mut last_err = None;
            for _ in 0..5 {
                match SyncEngine::push_outbox(&p, &client, &*cloud) {
                    Ok(()) if p.outbox_list().unwrap().is_empty() => {
                        last_err = None;
                        break;
                    }
                    Ok(()) => {}
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
            assert!(
                last_err.is_none(),
                "{}: push failed under concurrency: {:?}",
                cf.name,
                last_err
            );
        }));
    }
    for h in handles {
        h.join().expect("writer thread panicked");
    }

    // Every file must be present + byte-for-byte on a fresh reader (one snapshot sees all).
    let reader = dev();
    SyncEngine::pull_snapshot(&reader, &cb).expect("reader snapshot");
    let mut verified = 0usize;
    let mut total_bytes = 0usize;
    for cf in files.iter() {
        let bf = reader
            .lookup(ROOT_INO, cf.name)
            .unwrap_or_else(|_| panic!("concurrent storm DROPPED file {}", cf.name));
        let got = if reader.getattr(bf.ino).unwrap().size == 0 {
            Vec::new()
        } else {
            let nid = reader.node_id_for(bf.ino).unwrap().unwrap();
            SyncEngine::hydrate(&reader, &cb, &*cloud, &nid)
                .unwrap_or_else(|e| panic!("hydrate {} after storm: {e}", cf.name));
            read_all(&reader, bf.ino)
        };
        assert_eq!(
            got, cf.bytes,
            "{}: corrupted under concurrent multi-type storm",
            cf.name
        );
        verified += 1;
        total_bytes += cf.bytes.len();
    }
    eprintln!(
        "\n==== CONCURRENT MULTI-FILETYPE STORM ====\n{verified} filetypes pushed by {verified} concurrent threads into one library, \
         all {} bytes ({:.1} MB) verified byte-for-byte on a fresh reader — no drops, no corruption\n",
        total_bytes,
        total_bytes as f64 / 1_048_576.0
    );
}
