//! The materializer (§3.4): the CAS ↔ filesystem bridge.
//!
//! Answers *where do the bytes go?* It prepares a fresh sandbox root, hardlinks
//! declared inputs from the CAS into it (via [`Cas::link_into`], which owns the
//! hardlink-vs-copy mechanism), and after execution captures declared outputs back
//! into the CAS. It knows nothing about isolation — that is the sandbox's job.
//!
//! Path convention: the command runs with cwd = `<root>/<working_directory>`; input
//! and output paths are relative to that cwd. `HOME` and `TMPDIR` are dedicated
//! subdirectories of the root so tools have somewhere to write.
//!
//! [`Cas::link_into`]: anneal_cas::Cas::link_into

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anneal_cas::Cas;
use anneal_core::Digest;

use crate::action::Action;
use crate::executor::ExecError;

static SANDBOX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A prepared sandbox: directories created and inputs materialized, ready to run.
pub(crate) struct PreparedSandbox {
    /// Root of the per-action sandbox (removed after the run unless retained).
    pub root: PathBuf,
    /// The command's working directory (`root/working_directory`).
    pub cwd: PathBuf,
    /// Canonical `HOME` for the action.
    pub home: PathBuf,
    /// Canonical `TMPDIR` for the action.
    pub tmp: PathBuf,
}

/// Create a fresh sandbox under `base` and materialize all declared inputs into it.
/// The directory name embeds the action key so retained sandboxes are identifiable.
pub(crate) fn prepare(
    cas: &Cas,
    action: &Action,
    base: &Path,
    key: &Digest,
) -> Result<PreparedSandbox, ExecError> {
    let nonce = SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = base.join(format!("{}-{}", &key.to_hex()[..16], nonce));
    // Joining a bare "." would append a CurDir component that `create_dir_all`
    // rejects, so treat the default working directory as the root itself.
    let cwd = if action.working_directory == Path::new(".") {
        root.clone()
    } else {
        root.join(&action.working_directory)
    };
    let home = root.join(".home");
    let tmp = root.join(".tmp");

    std::fs::create_dir_all(&cwd).map_err(ExecError::Io)?;
    std::fs::create_dir_all(&home).map_err(ExecError::Io)?;
    std::fs::create_dir_all(&tmp).map_err(ExecError::Io)?;

    for input in action.inputs.values() {
        let dest = cwd.join(&input.path);
        cas.link_into(&input.digest, &dest).map_err(ExecError::Io)?;
    }

    Ok(PreparedSandbox {
        root,
        cwd,
        home,
        tmp,
    })
}

/// Read each declared output from the sandbox and store it in the CAS, returning the
/// logical-name → content-digest map. A missing declared output is an action error.
pub(crate) fn capture(
    cas: &Cas,
    action: &Action,
    prepared: &PreparedSandbox,
) -> Result<BTreeMap<String, Digest>, ExecError> {
    let mut outputs = BTreeMap::new();
    for (name, rel_path) in &action.outputs {
        let path = prepared.cwd.join(rel_path);
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ExecError::MissingOutput(name.clone())
            } else {
                ExecError::Io(e)
            }
        })?;
        let digest = cas.put(&bytes).map_err(ExecError::Io)?;
        outputs.insert(name.clone(), digest);
    }
    Ok(outputs)
}
