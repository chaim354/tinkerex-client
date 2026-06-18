//! Repro for "copying two files at once truncates the second" (reported on the macOS WebDAV
//! mount: the 2nd of two 11 MB files committed only ~1 KB). Mirror macOS WebDAV's per-file
//! write sequence — create -> truncate(ino,0) [the O_TRUNC open] -> many write(ino, off, chunk)
//! -> flush — and run two of them AT ONCE on a shared provider. Each file must read back its
//! FULL content. Looped to surface a flaky interleaving.

mod common;
use common::*;

use std::sync::Arc;
use std::thread;

use fs_provider::Provider;

#[test]
fn concurrent_webdav_style_copy_keeps_full_content() {
    for round in 0..40 {
        let p = Arc::new(provider());
        let size: usize = 300 * 1024; // 300 KB → many chunks under the small test ChunkConfig
        let chunk: usize = 4096; // WebDAV write_buf-ish chunking

        // Distinct content per file so cross-file bleed is detectable, varied per round.
        let bodies: Vec<Vec<u8>> = (0..2u64)
            .map(|t| (0..size).map(|b| ((b + (t as usize) * 7 + round * 13) % 251) as u8).collect())
            .collect();

        let mut handles = Vec::new();
        for t in 0..2u64 {
            let p = Arc::clone(&p);
            let body = bodies[t as usize].clone();
            handles.push(thread::spawn(move || {
                let name = format!("file{t}.bin");
                let f = p.create(ROOT, &name).unwrap();
                p.truncate(f.ino, 0).unwrap(); // the kernel's O_TRUNC open
                let mut off = 0u64;
                while (off as usize) < body.len() {
                    let end = ((off as usize) + chunk).min(body.len());
                    let n = p.write(f.ino, off, &body[off as usize..end]).unwrap();
                    off += n as u64;
                }
                p.flush(f.ino).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        for t in 0..2u64 {
            let ino = p.lookup(ROOT, &format!("file{t}.bin")).unwrap().ino;
            let got = read_all(&p, ino);
            assert_eq!(got.len(), bodies[t as usize].len(), "round {round}: file{t} LENGTH");
            assert_eq!(got, bodies[t as usize], "round {round}: file{t} content intact");
        }
    }
}
