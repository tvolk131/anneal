//! The action cache (§8.1): cache-key computation and a persistent
//! action-digest → result map.
//!
//! This is where cache-key *hashing* lives (the deep module that owns it). It pulls
//! canonical data from `anneal-core` — notably [`AxisValues::consumed`] for axis
//! trimming — and folds it into a single content [`Digest`].
//!
//! [`AxisValues::consumed`]: anneal_core::AxisValues::consumed

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anneal_core::Digest;

use crate::action::{Action, InputSource};
use crate::trust::{CacheTier, EnforcementGrade, Provenance};
use crate::SANDBOX_VERSION;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compute the **action digest** — the cache key (§8.1).
///
/// Folds in: a version tag, the sandbox version, the command, declared input
/// (name, path, content-digest) triples, the env map (keys **and** values),
/// working directory, execution mode, cache policy, the target triple ("relevant
/// platform requirements"), and **only the consumed configuration axes** (trimming).
///
/// Deliberately excluded: timestamps, the action *name*, and the host environment.
///
/// Encoding is length-prefixed so no two distinct field sequences can collide.
pub fn action_digest(action: &Action) -> Digest {
    let mut buf = Vec::new();

    write_str(&mut buf, "anneal-action-v1");
    write_str(&mut buf, SANDBOX_VERSION);

    // command (ordered argv)
    write_count(&mut buf, action.command.len());
    for arg in &action.command {
        write_str(&mut buf, arg);
    }

    // native fetch URL. Written only when present, so the digests of ordinary
    // (command) actions are unchanged by the field's introduction. (FixedOutput
    // results are cached by output, not by this digest — included for the
    // totality of action identity, not for cache correctness.)
    if let Some(url) = &action.fetch_url {
        write_str(&mut buf, "fetch-url");
        write_str(&mut buf, url);
    }

    // inputs (BTreeMap → sorted by name). The source is tagged so a Blob digest can
    // never collide with an Output reference. In normal execution every input is a
    // Blob by the time keying happens (the graph executor resolves Output refs to
    // Blobs first); the Output arm is kept for totality.
    write_count(&mut buf, action.inputs.len());
    for (name, input) in &action.inputs {
        write_str(&mut buf, name);
        write_str(&mut buf, &input.path.to_string_lossy());
        buf.push(u8::from(input.writable));
        match &input.source {
            InputSource::Blob(digest) => {
                buf.push(0);
                write_bytes(&mut buf, digest.as_bytes());
            }
            InputSource::Output { action, name } => {
                buf.push(1);
                write_str(&mut buf, action);
                write_str(&mut buf, name);
            }
        }
    }

    // env (BTreeMap → sorted by key); keys and values both matter (§7.4)
    write_count(&mut buf, action.env.len());
    for (key, value) in &action.env {
        write_str(&mut buf, key);
        write_str(&mut buf, value);
    }

    // toolchain identities (BTreeMap → sorted by name). The identity is the cache
    // boundary; roots/bin dirs are included too so sandbox policy-relevant mount
    // hints cannot drift without changing the key.
    write_count(&mut buf, action.toolchains.len());
    for (name, toolchain) in &action.toolchains {
        write_str(&mut buf, name);
        write_str(&mut buf, toolchain.identity());
        write_count(&mut buf, toolchain.bin_dirs().len());
        for dir in toolchain.bin_dirs() {
            write_str(&mut buf, &dir.to_string_lossy());
        }
        write_count(&mut buf, toolchain.read_only_roots().len());
        for root in toolchain.read_only_roots() {
            write_str(&mut buf, &root.to_string_lossy());
        }
    }

    write_str(&mut buf, &action.working_directory.to_string_lossy());
    write_str(&mut buf, action.execution_mode.as_str());
    write_str(&mut buf, action.cache_policy.as_str());

    // The declared snapshot paths are part of the key (§19.1); the snapshot *key*
    // itself is NOT — a snapshot is a correctness-neutral accelerator (§8.2).
    write_count(&mut buf, action.snapshot_paths.len());
    for path in &action.snapshot_paths {
        write_str(&mut buf, &path.to_string_lossy());
    }

    // relevant platform requirements: the target triple for platform-sensitive
    // actions, a fixed marker for platform-independent ones (so their key — and thus
    // their cached result — is shared across all platforms, §6.3).
    if action.platform_sensitive {
        write_str(&mut buf, action.config.platform().target_triple());
    } else {
        write_str(&mut buf, "*platform-independent*");
    }

    // consumed axes only (trimming, §6.2), in canonical order
    let consumed = action.config.axes().consumed(&action.consumed_axes);
    write_count(&mut buf, consumed.len());
    for (axis, value) in consumed {
        write_str(&mut buf, axis);
        write_str(&mut buf, value);
    }

    Digest::of(&buf)
}

fn write_count(buf: &mut Vec<u8>, n: usize) {
    buf.extend_from_slice(&(n as u64).to_le_bytes());
}

fn write_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_count(buf, bytes.len());
    buf.extend_from_slice(bytes);
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_bytes(buf, s.as_bytes());
}

/// The persisted result of a successful action: exit code, output digests, and
/// the provenance of the run that produced it (DESIGN.md §2.8 — producing
/// platform, enforcement grade, computed tier). Provenance is `Option` only to
/// tolerate entries written before it existed; new inserts always carry it.
/// (Only successful actions are stored — "save on success only", §8.5.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredResult {
    pub exit_code: i32,
    pub outputs: BTreeMap<String, Digest>,
    pub provenance: Option<Provenance>,
}

/// A persistent map from action digest to [`StoredResult`], stored as small
/// prefix-sharded text files under a root directory.
pub(crate) struct ActionCache {
    dir: PathBuf,
}

impl ActionCache {
    pub(crate) fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = root.into();
        fs::create_dir_all(&dir)?;
        Ok(ActionCache { dir })
    }

    pub(crate) fn lookup(&self, key: &Digest) -> io::Result<Option<StoredResult>> {
        match fs::read_to_string(self.entry_path(key)) {
            Ok(text) => Ok(Some(parse_entry(&text)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn insert(&self, key: &Digest, result: &StoredResult) -> io::Result<()> {
        let path = self.entry_path(key);
        let shard = path.parent().expect("entry path always has a shard parent");
        fs::create_dir_all(shard)?;
        let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = shard.join(format!(".tmp.{}.{}", std::process::id(), nonce));
        fs::write(&tmp, serialize_entry(result))?;
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
        self.dir.join(&hex[..2]).join(&hex[2..])
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
    use crate::action::{Action, CachePolicy, ExecutionMode, Toolchain};
    use anneal_core::{Axis, AxisValues, Configuration, OptLevel, Platform};

    fn cfg(opt: OptLevel) -> Configuration {
        Configuration::new(
            Platform::new("host", "x86_64-host"),
            AxisValues {
                opt_level: opt,
                ..Default::default()
            },
        )
    }

    #[test]
    fn name_is_excluded_from_key() {
        let a = Action::builder("name-a", ["./true"]).build();
        let b = Action::builder("name-b", ["./true"]).build();
        assert_eq!(action_digest(&a), action_digest(&b));
    }

    #[test]
    fn command_and_env_change_the_key() {
        let base = Action::builder("a", ["./echo", "x"]).build();
        let diff_cmd = Action::builder("a", ["./echo", "y"]).build();
        let diff_env = Action::builder("a", ["./echo", "x"]).env("K", "V").build();
        assert_ne!(action_digest(&base), action_digest(&diff_cmd));
        assert_ne!(action_digest(&base), action_digest(&diff_env));
    }

    #[test]
    fn writable_inputs_change_the_key() {
        let d = Digest::of(b"manifest");
        let readonly = Action::builder("a", ["./tool"])
            .input("manifest", "manifest.txt", d)
            .build();
        let writable = Action::builder("a", ["./tool"])
            .writable_input("manifest", "manifest.txt", d)
            .build();
        assert_ne!(action_digest(&readonly), action_digest(&writable));
    }

    #[test]
    fn unconsumed_axis_does_not_change_key_but_consumed_one_does() {
        // Same action, configs differ only in opt_level.
        let make = |opt, consume: &[Axis]| {
            Action::builder("a", ["./true"])
                .configured(cfg(opt), consume.to_vec())
                .build()
        };
        // opt_level NOT consumed → trimmed out → keys equal.
        assert_eq!(
            action_digest(&make(OptLevel::Debug, &[])),
            action_digest(&make(OptLevel::Release, &[])),
        );
        // opt_level consumed → keys differ.
        assert_ne!(
            action_digest(&make(OptLevel::Debug, &[Axis::OptLevel])),
            action_digest(&make(OptLevel::Release, &[Axis::OptLevel])),
        );
    }

    #[test]
    fn consumed_axis_order_and_duplicates_do_not_change_the_key() {
        let a = Action::builder("a", ["./true"])
            .configured(cfg(OptLevel::Release), [Axis::Coverage, Axis::OptLevel])
            .build();
        let b = Action::builder("a", ["./true"])
            .configured(
                cfg(OptLevel::Release),
                [Axis::OptLevel, Axis::Coverage, Axis::Coverage],
            )
            .build();
        assert_eq!(action_digest(&a), action_digest(&b));
    }

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
    fn mode_and_policy_change_the_key() {
        let base = Action::builder("a", ["./true"]).build();
        let permeable = Action::builder("a", ["/bin/true"])
            .mode(ExecutionMode::Permeable)
            .build();
        let noncache = Action::builder("a", ["./true"])
            .cache_policy(CachePolicy::NonCacheable)
            .build();
        assert_ne!(action_digest(&base), action_digest(&permeable));
        assert_ne!(action_digest(&base), action_digest(&noncache));
    }

    #[test]
    fn toolchain_identity_changes_the_key() {
        let toolchain = |identity| {
            Toolchain::new(
                "rust",
                identity,
                vec![PathBuf::from("/nix/store/rust/bin")],
                vec![PathBuf::from("/nix/store/rust")],
            )
            .unwrap()
        };
        let a = Action::builder("a", ["/nix/store/rust/bin/true"])
            .toolchain(toolchain("/nix/store/rust-a/bin/cargo"))
            .build();
        let b = Action::builder("a", ["/nix/store/rust/bin/true"])
            .toolchain(toolchain("/nix/store/rust-b/bin/cargo"))
            .build();

        assert_ne!(action_digest(&a), action_digest(&b));
    }
}
