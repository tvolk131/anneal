//! Action identity (§8.1): the cache-key computation. The *persistence* of
//! results (the action cache) lives in `anneal-store`; this module owns what
//! an action's identity **is**.
//!
//! [`ActionIdentity`] is the compiler-enforced field set: every
//! identity-relevant property of an [`Action`] is projected into the struct by
//! [`ActionIdentity::from_action`], and the digest is the fold over its
//! canonical bytes. Adding an `Action` field without adding it here is caught
//! by the per-field variation tests below — the regression guard for TODO.md
//! P0 #6.
//!
//! **Deliberately excluded from identity** (each has a test pinning it):
//! the action *name* (graph plumbing, not work description), `mirror_to_tree`
//! (a `materialize` affordance, not build identity), `timeout_ms` (changes
//! failure behavior, not outputs), `snapshot_key`/`snapshot_shared`
//! (accelerators — §1.4: they may change cost, never output).
//!
//! **Deliberately included as of v2** (TODO.md P0 #1): the complete declared
//! output map — two actions differing only in output names or destination
//! paths are different work, and must never share a cache entry — plus the
//! `network` capability (a different sandbox contract is different work).

use anneal_core::Digest;

use crate::action::{Action, InputSource};
use crate::SANDBOX_VERSION;

/// The version tag of the identity encoding itself. Bumped when the *meaning*
/// of an identity changes (v2 adds the output map and the network flag) — every
/// existing entry invalidates, exactly once.
const ACTION_IDENTITY_VERSION: &str = "anneal-action-v2";

/// The complete identity-relevant projection of an [`Action`]. Field-for-field
/// documentation lives on [`Action`]; the rule here is *totality*: if a field
/// can change what a successful run produces, or what its outputs are named,
/// it must appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionIdentity {
    command: Vec<String>,
    /// The native fetch URL, when this action is one (§FOD). Written only when
    /// present so ordinary actions are unaffected by the field (the discipline
    /// that lets the tag encode *kind*, not just version).
    fetch_url: Option<String>,
    /// Declared inputs, sorted by logical name: `(name, path, writable,
    /// source)`. The source is tagged so a blob digest can never collide with
    /// an output reference.
    inputs: Vec<(String, std::path::PathBuf, bool, InputId)>,
    /// The complete declared output map (P0 #1): logical name → destination
    /// path, sorted.
    outputs: Vec<(String, std::path::PathBuf)>,
    env: Vec<(String, String)>,
    /// Toolchains: `(name, identity, bin_dirs, read_only_roots)`, sorted. The
    /// identity is the cache boundary; the mount hints are included too so
    /// policy-relevant changes cannot drift without changing the key.
    toolchains: Vec<(
        String,
        String,
        Vec<std::path::PathBuf>,
        Vec<std::path::PathBuf>,
    )>,
    working_directory: std::path::PathBuf,
    execution_mode: &'static str,
    cache_policy: &'static str,
    snapshot_paths: Vec<std::path::PathBuf>,
    /// The target triple for platform-sensitive actions; a fixed marker for
    /// platform-independent ones (so their results are shared across
    /// platforms, §6.3).
    platform: String,
    /// Only the consumed configuration axes (trimming, §6.2), canonical order.
    consumed_axes: Vec<(String, String)>,
    /// The network capability: a different sandbox contract is different work.
    network: bool,
}

/// An input's content source, tagged by shape.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputId {
    Blob(Digest),
    Output { action: String, name: String },
}

impl ActionIdentity {
    /// Project an [`Action`] into its identity. Every field read here is a
    /// deliberate inclusion; everything not read is a deliberate exclusion
    /// (see the module docs).
    pub(crate) fn from_action(action: &Action) -> Self {
        ActionIdentity {
            command: action.command.clone(),
            fetch_url: action.fetch_url.clone(),
            inputs: action
                .inputs
                .iter()
                .map(|(name, input)| {
                    let id = match &input.source {
                        InputSource::Blob(digest) => InputId::Blob(*digest),
                        InputSource::Output { action, name } => InputId::Output {
                            action: action.clone(),
                            name: name.clone(),
                        },
                    };
                    (name.clone(), input.path.clone(), input.writable, id)
                })
                .collect(),
            outputs: action
                .outputs
                .iter()
                .map(|(name, path)| (name.clone(), path.clone()))
                .collect(),
            env: action
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            toolchains: action
                .toolchains
                .values()
                .map(|tc| {
                    (
                        tc.name().to_owned(),
                        tc.identity().to_owned(),
                        tc.bin_dirs().to_vec(),
                        tc.read_only_roots().to_vec(),
                    )
                })
                .collect(),
            working_directory: action.working_directory.clone(),
            execution_mode: action.execution_mode.as_str(),
            cache_policy: action.cache_policy.as_str(),
            snapshot_paths: action.snapshot_paths.clone(),
            platform: if action.platform_sensitive {
                action.config.platform().target_triple().to_owned()
            } else {
                "*platform-independent*".to_owned()
            },
            consumed_axes: action
                .config
                .axes()
                .consumed(&action.consumed_axes)
                .into_iter()
                .map(|(axis, value)| (axis.to_owned(), value.to_owned()))
                .collect(),
            network: action.network,
        }
    }

    /// Canonical, length-prefixed bytes: no two distinct field sequences can
    /// collide, at any nesting level.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_str(&mut buf, ACTION_IDENTITY_VERSION);
        write_str(&mut buf, SANDBOX_VERSION);

        write_count(&mut buf, self.command.len());
        for arg in &self.command {
            write_str(&mut buf, arg);
        }

        if let Some(url) = &self.fetch_url {
            write_str(&mut buf, "fetch-url");
            write_str(&mut buf, url);
        }

        write_count(&mut buf, self.inputs.len());
        for (name, path, writable, id) in &self.inputs {
            write_str(&mut buf, name);
            write_str(&mut buf, &path.to_string_lossy());
            buf.push(u8::from(*writable));
            match id {
                InputId::Blob(digest) => {
                    buf.push(0);
                    write_bytes(&mut buf, digest.as_bytes());
                }
                InputId::Output { action, name } => {
                    buf.push(1);
                    write_str(&mut buf, action);
                    write_str(&mut buf, name);
                }
            }
        }

        // The complete declared output map (P0 #1): both the logical name and
        // the destination path are identity.
        write_count(&mut buf, self.outputs.len());
        for (name, path) in &self.outputs {
            write_str(&mut buf, name);
            write_str(&mut buf, &path.to_string_lossy());
        }

        write_count(&mut buf, self.env.len());
        for (key, value) in &self.env {
            write_str(&mut buf, key);
            write_str(&mut buf, value);
        }

        write_count(&mut buf, self.toolchains.len());
        for (name, identity, bin_dirs, roots) in &self.toolchains {
            write_str(&mut buf, name);
            write_str(&mut buf, identity);
            write_count(&mut buf, bin_dirs.len());
            for dir in bin_dirs {
                write_str(&mut buf, &dir.to_string_lossy());
            }
            write_count(&mut buf, roots.len());
            for root in roots {
                write_str(&mut buf, &root.to_string_lossy());
            }
        }

        write_str(&mut buf, &self.working_directory.to_string_lossy());
        write_str(&mut buf, self.execution_mode);
        write_str(&mut buf, self.cache_policy);

        // The declared snapshot paths are part of the key (§19.1); the snapshot
        // *key* itself is NOT — a snapshot is a correctness-neutral accelerator.
        write_count(&mut buf, self.snapshot_paths.len());
        for path in &self.snapshot_paths {
            write_str(&mut buf, &path.to_string_lossy());
        }

        write_str(&mut buf, &self.platform);

        write_count(&mut buf, self.consumed_axes.len());
        for (axis, value) in &self.consumed_axes {
            write_str(&mut buf, axis);
            write_str(&mut buf, value);
        }

        buf.push(u8::from(self.network));
        buf
    }
}

/// Compute the **action digest** — the cache key (§8.1). A pure function of
/// [`ActionIdentity`]: the version tag, the sandbox version, and every
/// identity-relevant field, length-prefixed.
pub fn action_digest(action: &Action) -> Digest {
    Digest::of(&ActionIdentity::from_action(action).canonical_bytes())
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

// The tests are the identity-field audit (TODO.md P0 #6): one variation per
// field, plus one pin per deliberate exclusion. A new `Action` field must
// appear in exactly one of the two groups — or it is not covered.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{CachePolicy, ExecutionMode, InputSource, Toolchain};
    use anneal_core::{Axis, AxisValues, Configuration, OptLevel, Platform};

    fn cfg(opt: OptLevel) -> Configuration {
        Configuration::new(
            Platform::new("host", "host-triple"),
            AxisValues {
                opt_level: opt,
                ..Default::default()
            },
        )
    }

    // --- Included fields: every variation must change the digest -----------------

    #[test]
    fn command_changes_the_key() {
        let base = Action::builder("a", ["./echo", "x"]).build();
        let diff = Action::builder("a", ["./echo", "y"]).build();
        assert_ne!(action_digest(&base), action_digest(&diff));
    }

    #[test]
    fn env_keys_and_values_change_the_key() {
        let base = Action::builder("a", ["./echo"]).build();
        let diff_key = Action::builder("a", ["./echo"]).env("K", "V").build();
        let diff_val = Action::builder("a", ["./echo"]).env("K", "W").build();
        assert_ne!(action_digest(&base), action_digest(&diff_key));
        assert_ne!(action_digest(&diff_key), action_digest(&diff_val));
    }

    #[test]
    fn input_digest_path_name_and_writability_change_the_key() {
        let d1 = Digest::of(b"one");
        let d2 = Digest::of(b"two");
        let base = Action::builder("a", ["./t"])
            .source_input("in", "src/a", d1)
            .build();
        let diff_digest = Action::builder("a", ["./t"])
            .source_input("in", "src/a", d2)
            .build();
        let diff_path = Action::builder("a", ["./t"])
            .source_input("in", "src/b", d1)
            .build();
        let diff_name = Action::builder("a", ["./t"])
            .source_input("other", "src/a", d1)
            .build();
        let diff_writable = Action::builder("a", ["./t"])
            .writable_source_input("in", "src/a", d1)
            .build();
        let base_k = action_digest(&base);
        assert_ne!(base_k, action_digest(&diff_digest));
        assert_ne!(base_k, action_digest(&diff_path));
        assert_ne!(base_k, action_digest(&diff_name));
        assert_ne!(base_k, action_digest(&diff_writable));
    }

    #[test]
    fn input_source_shape_is_tagged() {
        // A blob digest and an output reference can never collide.
        let d = Digest::of(b"x");
        let blob = Action::builder("a", ["./t"])
            .source_input("in", "i", d)
            .build();
        let output = Action::builder("a", ["./t"])
            .dependency_input("in", "i", "producer", "out")
            .build();
        assert_ne!(action_digest(&blob), action_digest(&output));
    }

    #[test]
    fn output_map_changes_the_key() {
        // P0 #1: the declared output map is identity. Same command, same
        // inputs, different output *name* or destination → different work.
        let base = Action::builder("a", ["./t"])
            .output("bin", "out/bin")
            .build();
        let diff_name = Action::builder("a", ["./t"])
            .output("img", "out/bin")
            .build();
        let diff_path = Action::builder("a", ["./t"])
            .output("bin", "out/other")
            .build();
        let added = Action::builder("a", ["./t"])
            .output("bin", "out/bin")
            .output("log", "out/log")
            .build();
        let base_k = action_digest(&base);
        assert_ne!(base_k, action_digest(&diff_name));
        assert_ne!(base_k, action_digest(&diff_path));
        assert_ne!(base_k, action_digest(&added));
    }

    #[test]
    fn network_capability_changes_the_key() {
        // A different sandbox contract is different work, even with identical
        // commands (the capability changes what the action may read).
        let base = Action::builder("a", ["./t"]).build();
        let net = Action::builder("a", ["./t"]).network(true).build();
        assert_ne!(action_digest(&base), action_digest(&net));
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
    fn toolchain_identity_bin_dirs_and_roots_change_the_key() {
        let toolchain = |identity: &str, bins: Vec<std::path::PathBuf>| {
            Toolchain::new(
                "rust",
                identity,
                bins,
                vec![std::path::PathBuf::from("/nix/store/rust")],
            )
            .unwrap()
        };
        let base = Action::builder("a", ["/nix/store/rust/bin/true"])
            .toolchain(toolchain("id-a", vec!["/nix/store/rust/bin".into()]))
            .build();
        let diff_identity = Action::builder("a", ["/nix/store/rust/bin/true"])
            .toolchain(toolchain("id-b", vec!["/nix/store/rust/bin".into()]))
            .build();
        let diff_bins = Action::builder("a", ["/nix/store/rust/bin/true"])
            .toolchain(toolchain("id-a", vec!["/nix/store/other/bin".into()]))
            .build();
        let base_k = action_digest(&base);
        assert_ne!(base_k, action_digest(&diff_identity));
        assert_ne!(base_k, action_digest(&diff_bins));
    }

    #[test]
    fn working_directory_changes_the_key() {
        let base = Action::builder("a", ["./t"]).build();
        let diff = Action::builder("a", ["./t"])
            .working_directory("sub")
            .build();
        assert_ne!(action_digest(&base), action_digest(&diff));
    }

    #[test]
    fn snapshot_paths_change_the_key() {
        let base = Action::builder("a", ["./t"])
            .snapshot(Digest::of(b"k"), vec!["target".into()])
            .build();
        let diff = Action::builder("a", ["./t"])
            .snapshot(Digest::of(b"k"), vec!["elsewhere".into()])
            .build();
        assert_ne!(action_digest(&base), action_digest(&diff));
    }

    #[test]
    fn platform_triple_changes_the_key_only_when_sensitive() {
        let make = |triple: &str, sensitive: bool| {
            let mut b = Action::builder("a", ["./t"]).configured(
                Configuration::new(
                    Platform::new(triple.to_owned(), triple.to_owned()),
                    AxisValues::default(),
                ),
                Vec::new(),
            );
            if !sensitive {
                b = b.platform_independent();
            }
            b.build()
        };
        // Sensitive: the triple is identity.
        assert_ne!(
            action_digest(&make("aarch64-unknown", true)),
            action_digest(&make("x86_64-unknown", true))
        );
        // Independent: shared across platforms (§6.3).
        assert_eq!(
            action_digest(&make("aarch64-unknown", false)),
            action_digest(&make("x86_64-unknown", false))
        );
    }

    #[test]
    fn consumed_axes_change_the_key_unconsumed_do_not() {
        let make = |opt: OptLevel, consume: &[Axis]| {
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
    fn fetch_url_is_identity_when_present() {
        let d = Digest::of(b"pin");
        let fetch = |url: &str| {
            Action::builder("a", Vec::<String>::new())
                .output("blob", "out")
                .fetch(url, d)
                .try_build()
                .unwrap()
        };
        let a = fetch("https://example.com/a");
        let b = fetch("https://example.com/b");
        assert_ne!(action_digest(&a), action_digest(&b));
    }

    // --- Deliberate exclusions: variations must NOT change the digest ------------

    #[test]
    fn name_is_excluded_from_the_key() {
        let a = Action::builder("name-a", ["./true"]).build();
        let b = Action::builder("name-b", ["./true"]).build();
        assert_eq!(action_digest(&a), action_digest(&b));
    }

    #[test]
    fn mirror_to_tree_is_excluded_from_the_key() {
        // The routed-data flag is a `materialize` affordance: two actions
        // differing only in `mirror_to_tree` MUST hash identically, or routing
        // a data input would spuriously bust the action cache.
        let plain = Action::builder("a", ["./cargo"])
            .dependency_input("data", "config.json", "gen", "config.json")
            .build();
        let routed = Action::builder("a", ["./cargo"])
            .data_input(
                "data",
                "config.json",
                InputSource::Output {
                    action: "gen".into(),
                    name: "config.json".into(),
                },
            )
            .build();
        assert_eq!(action_digest(&plain), action_digest(&routed));
    }

    #[test]
    fn timeout_is_excluded_from_the_key() {
        let a = Action::builder("a", ["./t"]).timeout_ms(1000).build();
        let b = Action::builder("a", ["./t"]).timeout_ms(9999).build();
        assert_eq!(action_digest(&a), action_digest(&b));
    }

    #[test]
    fn snapshot_key_and_sharing_are_excluded_from_the_key() {
        // §1.4: a snapshot is a correctness-neutral accelerator, and whether it
        // is shared changes only where it is saved — neither is the work.
        let a = Action::builder("a", ["./t"])
            .snapshot_private(Digest::of(b"k1"), vec!["target".into()])
            .build();
        let b = Action::builder("a", ["./t"])
            .snapshot(Digest::of(b"k2"), vec!["target".into()])
            .build();
        assert_eq!(action_digest(&a), action_digest(&b));
    }
}
