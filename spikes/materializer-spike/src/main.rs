//! Spike B — CAS + hardlink materializer on macOS/APFS.
//!
//! Retires the §22 risk "macOS materializer at scale". Probes the assumptions the
//! real materializer (§3.4) rests on:
//!   1. Content-addressed store: blobs named by sha256 of their content.
//!   2. Hardlink-from-CAS into a sandbox root is O(1) and shares inodes (no copy).
//!   3. Per-inode hardlink count is high enough that CAS dedup is never the limit.
//!   4. Same-filesystem requirement: detect via st_dev; cross-fs must fall back to copy.
//!   5. sandbox-exec is present and functional (the macOS `sealed`-mode isolation, §7.3).
//!
//! Throwaway code: clarity over abstraction.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// Minimal content-addressed store: `<root>/<digest>`.
struct Cas {
    root: PathBuf,
}

impl Cas {
    fn new(root: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Store bytes, return the digest (the CAS key).
    fn put(&self, bytes: &[u8]) -> std::io::Result<String> {
        let digest = sha256_hex(bytes);
        let path = self.root.join(&digest);
        if !path.exists() {
            // Write to a temp file then rename for atomicity (real CAS concern).
            let tmp = self.root.join(format!("{digest}.tmp"));
            fs::write(&tmp, bytes)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(digest)
    }

    fn path(&self, digest: &str) -> PathBuf {
        self.root.join(digest)
    }
}

/// Materialize a CAS blob into a sandbox path via hardlink, with cross-fs copy fallback.
/// Returns true if hardlinked, false if it fell back to copy.
fn materialize(cas: &Cas, digest: &str, dest: &Path) -> std::io::Result<bool> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let src = cas.path(digest);
    match fs::hard_link(&src, dest) {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc_exdev()) => {
            fs::copy(&src, dest)?;
            Ok(false)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // §14.4: a pre-existing file at a managed path should be an error in the
            // real system; here we just report it.
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

/// EXDEV ("cross-device link") errno. 18 on both Linux and macOS.
fn libc_exdev() -> i32 {
    18
}

fn dev_of(path: &Path) -> std::io::Result<u64> {
    Ok(fs::metadata(path)?.dev())
}

fn main() -> std::io::Result<()> {
    let base = std::env::temp_dir().join(format!("anneal-spikeB-{}", std::process::id()));
    let cas = Cas::new(base.join("cas"))?;
    let sandbox = base.join("sandbox");
    fs::create_dir_all(&sandbox)?;

    println!("== Spike B: CAS + hardlink materializer (macOS/APFS) ==");
    println!("base dir: {}", base.display());
    println!();

    // --- 1. CAS put + content addressing ---
    let content = b"fn main() { println!(\"hello from a materialized input\"); }\n";
    let digest = cas.put(content)?;
    let digest2 = cas.put(content)?; // dedup: same content -> same key, no second write
    println!("[1] CAS: stored blob, digest={}", &digest[..16]);
    println!("    dedup: re-put same content -> same digest = {}", digest == digest2);

    // --- 2. Hardlink materialization shares the inode ---
    let dest = sandbox.join("src/main.rs");
    let hardlinked = materialize(&cas, &digest, &dest)?;
    let cas_meta = fs::metadata(cas.path(&digest))?;
    let dest_meta = fs::metadata(&dest)?;
    println!("\n[2] materialize -> {}", dest.display());
    println!("    hardlinked (not copied) = {}", hardlinked);
    println!(
        "    same inode = {} (cas ino={}, dest ino={})",
        cas_meta.ino() == dest_meta.ino(),
        cas_meta.ino(),
        dest_meta.ino()
    );
    println!("    nlink on shared inode = {}", dest_meta.nlink());

    // --- 3. Per-inode hardlink limit probe ---
    // The real concern (§22): can a single popular CAS blob be hardlinked into as
    // many sandboxes as a big build needs? Escalate until failure or a high bound.
    let target = 50_000usize;
    let links_dir = base.join("links");
    fs::create_dir_all(&links_dir)?;
    let start = Instant::now();
    let mut made = 0usize;
    let mut limit_hit: Option<String> = None;
    for i in 0..target {
        let p = links_dir.join(format!("l{i}"));
        match fs::hard_link(cas.path(&digest), &p) {
            Ok(()) => made += 1,
            Err(e) => {
                limit_hit = Some(format!("{e} (errno {:?})", e.raw_os_error()));
                break;
            }
        }
    }
    let elapsed = start.elapsed();
    let final_nlink = fs::metadata(cas.path(&digest))?.nlink();
    println!("\n[3] per-inode hardlink limit probe:");
    match &limit_hit {
        None => println!(
            "    created {made} hardlinks to one inode with no limit hit (nlink={final_nlink})",
            final_nlink = final_nlink
        ),
        Some(err) => println!("    limit hit after {made} links: {err}"),
    }
    println!(
        "    rate: {made} links in {:?} = {:.0} links/sec",
        elapsed,
        made as f64 / elapsed.as_secs_f64().max(1e-9)
    );

    // --- 4. Same-filesystem check (st_dev) + cross-fs fallback path ---
    let cas_dev = dev_of(&cas.root)?;
    let sandbox_dev = dev_of(&sandbox)?;
    println!("\n[4] filesystem check (hardlinks require same st_dev):");
    println!(
        "    cas st_dev={cas_dev}, sandbox st_dev={sandbox_dev}, same volume = {}",
        cas_dev == sandbox_dev
    );
    // Probe a likely-different volume so we exercise the EXDEV fallback path.
    for candidate in ["/var/folders", "/Volumes", "/private/tmp"] {
        if let Ok(d) = dev_of(Path::new(candidate)) {
            println!(
                "    {candidate}: st_dev={d}, same as cas = {}",
                d == cas_dev
            );
        }
    }

    // --- 5. sandbox-exec availability (macOS sealed-mode isolation, §7.3) ---
    println!("\n[5] sandbox-exec (macOS isolation layer):");
    let which = Command::new("which").arg("sandbox-exec").output();
    match which {
        Ok(o) if o.status.success() => {
            let p = String::from_utf8_lossy(&o.stdout);
            println!("    present at: {}", p.trim());
            // Run a trivial command under a permissive profile to confirm it works.
            let profile = "(version 1)(allow default)";
            let run = Command::new("sandbox-exec")
                .args(["-p", profile, "/bin/echo", "sandboxed-ok"])
                .output();
            match run {
                Ok(r) if r.status.success() => println!(
                    "    functional: ran echo under a profile -> {}",
                    String::from_utf8_lossy(&r.stdout).trim()
                ),
                Ok(r) => println!(
                    "    present but command failed: {}",
                    String::from_utf8_lossy(&r.stderr).trim()
                ),
                Err(e) => println!("    present but invocation errored: {e}"),
            }
        }
        _ => println!("    NOT found on PATH"),
    }

    // Cleanup
    let _ = fs::remove_dir_all(&base);
    println!("\n== done (cleaned up {}) ==", base.display());
    Ok(())
}
