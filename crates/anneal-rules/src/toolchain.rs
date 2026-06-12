use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anneal_exec::Toolchain;
use serde::Deserialize;

use crate::rule::RuleError;

const TOOLCHAIN_MANIFEST_ENV: &str = "ANNEAL_TOOLCHAIN_MANIFEST";

static TOOLCHAIN_MANIFEST: OnceLock<Result<ToolchainManifest, String>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct ToolchainManifest {
    version: u32,
    toolchains: BTreeMap<String, ManifestToolchain>,
}

#[derive(Debug, Deserialize)]
struct ManifestToolchain {
    tools: BTreeMap<String, PathBuf>,
    read_only_roots: Vec<PathBuf>,
}

/// The minimal shell/runtime surface used by first-party shell fragments.
///
/// This is intentionally small and explicit. Rule authors can use these tools in
/// first-party-generated scripts without depending on host `/bin` or `/usr/bin` on
/// Linux; the resolver below mounts the complete Nix closure read-only.
pub(crate) fn nix_base_runtime() -> Result<Toolchain, RuleError> {
    nix_store_toolchain(
        "posix-runtime",
        &[
            // `gzip` is not invoked directly by any rule script, but GNU tar's
            // `z` flag execs it from PATH (the §FOD vendor assembly).
            "sh", "cat", "chmod", "cp", "curl", "grep", "gzip", "head", "mkdir", "sed", "tar",
        ],
    )
}

/// Resolve a first-party rule toolchain from the Nix-generated manifest.
///
/// Every executable and mounted root must live under `/nix/store/...`. The
/// manifest makes that contract first-class and moves closure computation out of
/// the hot analysis loop.
pub(crate) fn nix_store_toolchain(name: &str, tools: &[&str]) -> Result<Toolchain, RuleError> {
    if tools.is_empty() {
        return Err(RuleError::Message(format!(
            "{name} toolchain requires at least one tool"
        )));
    }

    manifest_toolchain_from_manifest(toolchain_manifest()?, name, tools)
}

/// Build a PATH containing only declared toolchain bin directories.
pub(crate) fn toolchain_path_env(toolchains: &[&Toolchain]) -> String {
    let mut dirs = Vec::new();
    for toolchain in toolchains {
        for dir in toolchain.bin_dirs() {
            push_unique(&mut dirs, dir.clone());
        }
    }
    dirs.iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

fn manifest_toolchain_from_manifest(
    manifest: &ToolchainManifest,
    name: &str,
    tools: &[&str],
) -> Result<Toolchain, RuleError> {
    let declared = manifest.toolchains.get(name).ok_or_else(|| {
        RuleError::Message(format!(
            "toolchain manifest does not declare `{name}`; update the Nix-generated `{TOOLCHAIN_MANIFEST_ENV}` manifest"
        ))
    })?;

    let mut bin_dirs: Vec<PathBuf> = Vec::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in &declared.read_only_roots {
        let store_root = nix_store_root(root).ok_or_else(|| {
            RuleError::Message(format!(
                "{name} toolchain manifest root `{}` is not under `/nix/store/...`",
                root.display()
            ))
        })?;
        if store_root != *root {
            return Err(RuleError::Message(format!(
                "{name} toolchain manifest root `{}` is not a store root",
                root.display()
            )));
        }
        push_unique(&mut roots, root.clone());
    }
    roots.sort();

    let mut identity_parts: Vec<String> = Vec::new();
    for tool in tools {
        let executable = declared.tools.get(*tool).ok_or_else(|| {
            RuleError::Message(format!(
                "{name} toolchain manifest does not declare required tool `{tool}`"
            ))
        })?;
        if !executable.is_absolute() {
            return Err(RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` as `{}`, but tool paths must be absolute",
                executable.display()
            )));
        }
        let executable_root = nix_store_root(executable).ok_or_else(|| {
            RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` as `{}`, but first-party rules require `/nix/store/...` tools",
                executable.display()
            ))
        })?;
        if !roots.contains(&executable_root) {
            return Err(RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` at `{}`, but does not mount `{}`",
                executable.display(),
                executable_root.display()
            )));
        }
        let resolved = fs::canonicalize(executable).map_err(|e| {
            RuleError::Message(format!(
                "resolving manifest-declared `{}` for {name} toolchain: {e}",
                executable.display()
            ))
        })?;
        let resolved_root = nix_store_root(&resolved).ok_or_else(|| {
            RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` as `{}`, which resolves to `{}` outside `/nix/store/...`",
                executable.display(),
                resolved.display()
            ))
        })?;
        if !roots.contains(&resolved_root) {
            return Err(RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` as `{}`, which resolves into `{}`, but that root is not mounted",
                executable.display(),
                resolved_root.display()
            )));
        }

        let bin_dir = executable
            .parent()
            .ok_or_else(|| {
                RuleError::Message(format!(
                    "{name} toolchain manifest declares `{tool}` as `{}`, which has no parent directory",
                    executable.display()
                ))
            })?
            .to_path_buf();
        if !bin_dir.join(tool).is_file() {
            return Err(RuleError::Message(format!(
                "{name} toolchain manifest declares `{tool}` as `{}`, but no executable named `{tool}` was found in `{}`",
                executable.display(),
                bin_dir.display()
            )));
        }

        push_unique(&mut bin_dirs, bin_dir);
        identity_parts.push(format!("{tool}={}", resolved.display()));
    }
    for root in &roots {
        identity_parts.push(format!("closure={}", root.display()));
    }

    Toolchain::new(name, identity_parts.join(";"), bin_dirs, roots)
        .map_err(|e| RuleError::Message(format!("invalid {name} toolchain manifest: {e}")))
}

fn toolchain_manifest() -> Result<&'static ToolchainManifest, RuleError> {
    match TOOLCHAIN_MANIFEST.get_or_init(load_toolchain_manifest) {
        Ok(manifest) => Ok(manifest),
        Err(message) => Err(RuleError::Message(message.clone())),
    }
}

fn load_toolchain_manifest() -> Result<ToolchainManifest, String> {
    let Some(path) = std::env::var_os(TOOLCHAIN_MANIFEST_ENV) else {
        return Err(missing_toolchain_manifest_message());
    };
    load_toolchain_manifest_from_path(PathBuf::from(path))
}

fn load_toolchain_manifest_from_path(path: PathBuf) -> Result<ToolchainManifest, String> {
    let contents = fs::read_to_string(&path).map_err(|e| {
        format!(
            "reading toolchain manifest from `{}` (`{TOOLCHAIN_MANIFEST_ENV}`): {e}",
            path.display()
        )
    })?;
    let manifest: ToolchainManifest = serde_json::from_str(&contents).map_err(|e| {
        format!(
            "parsing toolchain manifest from `{}` (`{TOOLCHAIN_MANIFEST_ENV}`): {e}",
            path.display()
        )
    })?;
    if manifest.version != 1 {
        return Err(format!(
            "unsupported toolchain manifest version {} from `{}`; expected version 1",
            manifest.version,
            path.display()
        ));
    }
    Ok(manifest)
}

fn missing_toolchain_manifest_message() -> String {
    format!(
        "`{TOOLCHAIN_MANIFEST_ENV}` must be set for first-party toolchain resolution; run under `nix develop` or set it to the Nix-built `.#toolchain-manifest` output"
    )
}

fn nix_store_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    if !matches!(components.next()?, std::path::Component::RootDir) {
        return None;
    }
    if components.next()?.as_os_str() != OsStr::new("nix") {
        return None;
    }
    if components.next()?.as_os_str() != OsStr::new("store") {
        return None;
    }
    let store_entry = components.next()?;
    Some(PathBuf::from("/nix/store").join(store_entry.as_os_str()))
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        manifest_toolchain_from_manifest, missing_toolchain_manifest_message, nix_store_root,
        nix_store_toolchain, toolchain_path_env, ManifestToolchain, ToolchainManifest,
        TOOLCHAIN_MANIFEST_ENV,
    };
    use anneal_exec::Toolchain;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn extracts_store_root_from_resolved_tool_path() {
        assert_eq!(
            nix_store_root(Path::new("/nix/store/abc-rust/bin/cargo")),
            Some(PathBuf::from("/nix/store/abc-rust"))
        );
        assert_eq!(nix_store_root(Path::new("/usr/bin/cargo")), None);
    }

    #[test]
    fn toolchain_path_env_deduplicates_bin_dirs_in_order() {
        let first = Toolchain::new(
            "first",
            "first-id",
            vec![PathBuf::from("/nix/store/a/bin")],
            vec![PathBuf::from("/nix/store/a")],
        )
        .unwrap();
        let second = Toolchain::new(
            "second",
            "second-id",
            vec![
                PathBuf::from("/nix/store/a/bin"),
                PathBuf::from("/nix/store/b/bin"),
            ],
            vec![PathBuf::from("/nix/store/b")],
        )
        .unwrap();

        assert_eq!(
            toolchain_path_env(&[&first, &second]),
            "/nix/store/a/bin:/nix/store/b/bin"
        );
    }

    #[test]
    fn manifest_toolchain_requires_declared_toolchain() {
        let manifest = ToolchainManifest {
            version: 1,
            toolchains: BTreeMap::new(),
        };

        let err = manifest_toolchain_from_manifest(&manifest, "rust", &["cargo"]).unwrap_err();

        assert!(err
            .to_string()
            .contains("toolchain manifest does not declare `rust`"));
    }

    #[test]
    fn manifest_toolchain_requires_all_tools() {
        let mut toolchains = BTreeMap::new();
        toolchains.insert(
            "rust".to_owned(),
            ManifestToolchain {
                tools: BTreeMap::new(),
                read_only_roots: vec![PathBuf::from("/nix/store/abc-rust")],
            },
        );
        let manifest = ToolchainManifest {
            version: 1,
            toolchains,
        };

        let err = manifest_toolchain_from_manifest(&manifest, "rust", &["cargo"]).unwrap_err();

        assert!(err
            .to_string()
            .contains("rust toolchain manifest does not declare required tool `cargo`"));
    }

    #[test]
    fn missing_toolchain_manifest_message_requires_env() {
        let message = missing_toolchain_manifest_message();

        assert!(message.contains("`ANNEAL_TOOLCHAIN_MANIFEST` must be set"));
        assert!(message.contains("nix develop"));
    }

    #[test]
    fn toolchain_resolution_requires_manifest_env_when_absent() {
        if std::env::var_os(TOOLCHAIN_MANIFEST_ENV).is_some() {
            return;
        }

        let err = nix_store_toolchain("posix-runtime", &["sh"]).unwrap_err();

        assert!(err
            .to_string()
            .contains("`ANNEAL_TOOLCHAIN_MANIFEST` must be set"));
    }

    #[test]
    fn nix_manifest_runtime_resolves_from_env_when_present() {
        if std::env::var_os(TOOLCHAIN_MANIFEST_ENV).is_none() {
            return;
        }

        let toolchain = nix_store_toolchain("posix-runtime", &["sh", "cat"]).unwrap();

        assert_eq!(toolchain.name(), "posix-runtime");
        assert!(toolchain.identity().contains("sh=/nix/store/"));
        assert!(toolchain
            .read_only_roots()
            .iter()
            .all(|root| nix_store_root(root).as_deref() == Some(root.as_path())));
    }
}
