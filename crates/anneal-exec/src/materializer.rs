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

use anneal_cas::Cas;
use anneal_core::Digest;

use crate::action::{Action, InputSource};
use crate::executor::ExecError;

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

/// Create a fresh sandbox at `root` and materialize all declared inputs into it.
/// The caller owns the directory naming (a nonce for normal actions, a stable name
/// for snapshot-based ones).
pub(crate) fn prepare_at(
    cas: &Cas,
    action: &Action,
    root: PathBuf,
) -> Result<PreparedSandbox, ExecError> {
    let prepared = layout(action, root)?;
    materialize_inputs(cas, action, &prepared.cwd)?;
    Ok(prepared)
}

/// Compute the sandbox layout and create its directories (`cwd`, `HOME`, `TMPDIR`, and
/// declared-output parents) **without** materializing inputs. `prepare_at` is this plus
/// input materialization; the warm-reuse path uses `layout` alone and then syncs only the
/// changed inputs in place (see `warm`).
pub(crate) fn layout(action: &Action, root: PathBuf) -> Result<PreparedSandbox, ExecError> {
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

    // Pre-create parent directories for declared outputs, so an action can write to a
    // nested output path (e.g. `gen/config.json`) without creating the dir itself.
    for output in action.outputs.values() {
        if let Some(parent) = cwd.join(output).parent() {
            std::fs::create_dir_all(parent).map_err(ExecError::Io)?;
        }
    }

    Ok(PreparedSandbox {
        root,
        cwd,
        home,
        tmp,
    })
}

/// Materialize all declared inputs of `action` into `cwd` (hardlink/clone from the CAS).
fn materialize_inputs(cas: &Cas, action: &Action, cwd: &Path) -> Result<(), ExecError> {
    for input in action.inputs.values() {
        // Inputs must be resolved to blobs before reaching the materializer; the
        // graph executor guarantees this. An unresolved Output is a caller error.
        let digest = match &input.source {
            InputSource::Blob(digest) => digest,
            InputSource::Output { action, name } => {
                return Err(ExecError::UnresolvedInput {
                    action: action.clone(),
                    output: name.clone(),
                })
            }
        };
        cas.link_into(digest, &cwd.join(&input.path))
            .map_err(ExecError::Io)?;
    }
    Ok(())
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
