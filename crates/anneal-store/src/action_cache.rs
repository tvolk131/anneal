//! The persistent action cache (§8.1): a map from action digest to the
//! recorded result of a successful run. Persistence lives here (the store
//! crate); the *key computation* — `action_digest` and `ActionIdentity` —
//! stays beside the `Action` model in `anneal-exec`, so this module is
//! deliberately digest-keyed and knows nothing about actions.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anneal_cas::Cas;
use anneal_core::Digest;

use crate::trust::{CacheTier, EnforcementGrade, Provenance};
use crate::Verify;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The persisted result of a successful action: exit code, output digests, and
/// the provenance of the run that produced it. Provenance is `Option` only to
/// tolerate entries written before it existed; new inserts always carry it.
/// (Only successful actions are stored — "save on success only", §8.5.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredResult {
    pub exit_code: i32,
    pub outputs: BTreeMap<String, Digest>,
    pub provenance: Option<Provenance>,
}

/// A persistent map from action digest to [`StoredResult`], stored as small
/// prefix-sharded text files under a root directory.
///
/// Crash safety: every insert is temp-file + atomic rename, and the entry is
/// written only *after* its output blobs are already in the CAS (the executor
/// captures before it records) — a pointer is never published before its
/// pointee. An interruption leaves, at worst, an orphaned blob.
pub struct ActionCache {
    dir: PathBuf,
}

impl ActionCache {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = root.into();
        fs::create_dir_all(&dir)?;
        Ok(ActionCache { dir })
    }

    /// Plain lookup: `None` if no entry exists. Makes no claim about the
    /// presence of the referenced blobs — callers that *act* on a hit should
    /// use [`ActionCache::lookup_verified`].
    pub fn lookup(&self, key: &Digest) -> io::Result<Option<StoredResult>> {
        match fs::read_to_string(self.entry_path(key)) {
            Ok(text) => Ok(Some(parse_entry(&text)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Lookup with use-time verification (§3.1 of the anneal-store proposal):
    /// a hit is honored only if its output blobs survive the requested
    /// [`Verify`] tier, and otherwise **fails open to a miss** — the caller
    /// re-executes. This is what makes GC bugs and torn imports cost time
    /// instead of correctness:
    ///
    /// - [`Verify::Stats`] — every declared output blob is present (`stat`).
    /// - [`Verify::Hash`] — every blob is re-read and re-hashed against its
    ///   name (the paranoid tier; the import path).
    pub fn lookup_verified(
        &self,
        key: &Digest,
        cas: &Cas,
        verify: Verify,
    ) -> io::Result<Option<StoredResult>> {
        let Some(stored) = self.lookup(key)? else {
            return Ok(None);
        };
        for digest in stored.outputs.values() {
            let present = match verify {
                Verify::Stats => cas.has(digest),
                Verify::Hash => cas
                    .get(digest)?
                    .is_some_and(|bytes| Digest::of(&bytes) == *digest),
            };
            if !present {
                // Blob missing (or lying, under Hash): an entry without its
                // goods is inert. Degrade to a miss rather than fail closed on
                // a torn store — same posture as the query path.
                return Ok(None);
            }
        }
        Ok(Some(stored))
    }

    /// Record a result under `key`. Atomic (temp + rename); a racing identical
    /// insert is success, not a conflict (content-addressing makes them equal).
    pub fn insert(&self, key: &Digest, result: &StoredResult) -> io::Result<()> {
        let path = self.entry_path(key);
        let shard = path.parent().expect("entry path always has a shard parent");
        fs::create_dir_all(shard)?;
        let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = shard.join(format!(".tmp.{}.{}", std::process::id(), nonce));
        fs::write(&tmp, serialize_entry(result))?;
        // Crash-injection point: after the entry bytes are written, before the
        // rename publishes them — the "orphaned entry tmp" crash state.
        anneal_core::crash_point("action-insert");
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                if path.exists() {
                    Ok(()) // raced with an identical insert; fine
                } else {
                    Err(e)
                }
            }
        }
    }

    fn entry_path(&self, key: &Digest) -> PathBuf {
        let hex = key.to_hex();
        let (shard, rest) = hex.split_at(2);
        self.dir.join(shard).join(rest)
    }
}

/// Serialize as one `exit <code>` line, an optional `prov <platform> <grade>
/// <tier>` line, then `out <name> <hex>` lines. Output names are logical
/// identifiers (no whitespace), so the format is unambiguous.
fn serialize_entry(result: &StoredResult) -> String {
    let mut s = format!("exit {}\n", result.exit_code);
    if let Some(prov) = &result.provenance {
        s.push_str(&format!(
            "prov {} {} {}\n",
            prov.platform,
            prov.grade.as_str(),
            prov.tier.as_str()
        ));
    }
    for (name, digest) in &result.outputs {
        s.push_str(&format!("out {} {}\n", name, digest.to_hex()));
    }
    s
}

fn parse_entry(text: &str) -> io::Result<StoredResult> {
    let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_owned());

    let mut lines = text.lines();
    let exit_line = lines.next().ok_or_else(|| invalid("empty cache entry"))?;
    let exit_code: i32 = exit_line
        .strip_prefix("exit ")
        .ok_or_else(|| invalid("missing `exit` line"))?
        .trim()
        .parse()
        .map_err(|_| invalid("bad exit code"))?;

    let mut outputs = BTreeMap::new();
    let mut provenance = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("prov ") {
            let mut parts = rest.split(' ');
            let (platform, grade, tier) = (parts.next(), parts.next(), parts.next());
            let (Some(platform), Some(grade), Some(tier)) = (platform, grade, tier) else {
                return Err(invalid("malformed `prov` line"));
            };
            provenance = Some(Provenance {
                platform: platform.to_owned(),
                grade: EnforcementGrade::parse(grade)
                    .ok_or_else(|| invalid("bad provenance grade"))?,
                tier: CacheTier::parse(tier).ok_or_else(|| invalid("bad provenance tier"))?,
            });
            continue;
        }
        let rest = line
            .strip_prefix("out ")
            .ok_or_else(|| invalid("expected `out` line"))?;
        let (name, hex) = rest
            .split_once(' ')
            .ok_or_else(|| invalid("malformed `out` line"))?;
        let digest = Digest::from_hex(hex).map_err(|_| invalid("bad output digest"))?;
        outputs.insert(name.to_owned(), digest);
    }

    Ok(StoredResult {
        exit_code,
        outputs,
        provenance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_entry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ActionCache::open(dir.path()).unwrap();
        let key = Digest::of(b"key");
        let mut outputs = BTreeMap::new();
        outputs.insert("bin".to_owned(), Digest::of(b"binary"));
        outputs.insert("log".to_owned(), Digest::of(b"log"));
        let stored = StoredResult {
            exit_code: 0,
            outputs,
            provenance: Some(Provenance {
                platform: "testos-testarch".to_owned(),
                grade: EnforcementGrade::Enforced,
                tier: CacheTier::Promotable,
            }),
        };

        assert_eq!(cache.lookup(&key).unwrap(), None);
        cache.insert(&key, &stored).unwrap();
        assert_eq!(cache.lookup(&key).unwrap(), Some(stored));
    }

    #[test]
    fn pre_provenance_entries_still_parse() {
        // Entries written before the `prov` line existed must remain readable;
        // they surface as `provenance: None`.
        let parsed = parse_entry(
            "exit 0\nout bin 2222222222222222222222222222222222222222222222222222222222222222\n",
        )
        .unwrap();
        assert_eq!(parsed.exit_code, 0);
        assert_eq!(parsed.provenance, None);
        assert_eq!(parsed.outputs.len(), 1);
    }

    #[test]
    fn lookup_verified_fails_open_when_blobs_are_missing() {
        // An entry whose blobs are absent from the CAS must degrade to a miss
        // under Stats verification — never surface as a usable hit.
        let dir = tempfile::tempdir().unwrap();
        let cache = ActionCache::open(dir.path().join("actions")).unwrap();
        let cas = Cas::open(dir.path().join("cas")).unwrap();
        let key = Digest::of(b"key");
        cache
            .insert(
                &key,
                &StoredResult {
                    exit_code: 0,
                    outputs: BTreeMap::from([("out".to_owned(), Digest::of(b"never stored"))]),
                    provenance: None,
                },
            )
            .unwrap();

        assert_eq!(
            cache.lookup_verified(&key, &cas, Verify::Stats).unwrap(),
            None
        );

        // With the blob present, the hit is honored again.
        let digest = cas.put(b"present").unwrap();
        cache
            .insert(
                &key,
                &StoredResult {
                    exit_code: 0,
                    outputs: BTreeMap::from([("out".to_owned(), digest)]),
                    provenance: None,
                },
            )
            .unwrap();
        assert!(cache
            .lookup_verified(&key, &cas, Verify::Stats)
            .unwrap()
            .is_some());
        assert!(cache
            .lookup_verified(&key, &cas, Verify::Hash)
            .unwrap()
            .is_some());
    }
}
