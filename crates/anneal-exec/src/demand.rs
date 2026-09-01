//! Demand-driven pruning: which actions *may* run for an operation.
//!
//! Demand is a **derived** fact, not a declared one. An operation names
//! terminal outputs — a build demands the requested target's exposed
//! providers; a test additionally demands its test-result outputs — and the
//! actions that may run are exactly those **reachable backward** from the
//! terminals through the execution-dependency edges. An action unreachable
//! from the terminals cannot contribute a byte to anything demanded: dropping
//! it is conservative by construction, not by judgment. The practical
//! consequence: `build //app` never runs a dependency's test actions, and
//! `test //app` runs *app's* tests, not its dependencies'.
//!
//! The walk traverses the same two edge kinds the scheduler orders by:
//!
//! - **data edges** — an input declared as [`InputSource::Output`] references
//!   its producer;
//! - **snapshot-owner edges** — a [`CachePolicy::SnapshotConsuming`] action
//!   restores state a [`CachePolicy::SnapshotBased`] owner saves under the
//!   same key. This edge is invisible in the inputs (a pnpm script's declared
//!   inputs never mention the install action), so the walk must add it or
//!   pruning would starve consumers of their `node_modules`.
//!
//! One consequence worth stating plainly: an action whose output is consumed
//! by nothing and exposed by no provider becomes **dead** — it never runs.
//! That is correct (it produces nothing demanded), and it is the honest
//! failure mode for side-effect-only actions, which should be providers if
//! their effect matters.

use std::collections::HashMap;

use anneal_action::{Action, CachePolicy, InputSource};

/// The logical output name the CLI treats as a test result (the same
/// convention `test`'s summary reads). A `test` operation's terminals are the
/// requested target's providers plus every output with this name.
pub const TEST_RESULT_OUTPUT: &str = "results.txt";

/// One demanded terminal: a named output of a named action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub action: String,
    pub output: String,
}

/// Select the demanded subgraph: every action reachable backward from
/// `terminals` through data edges and snapshot-owner edges, in the input
/// list's original order. Terminals naming actions absent from the list are
/// ignored (they cannot demand what does not exist; the graph is internally
/// consistent, so this cannot fire in practice).
pub fn demanded(actions: &[Action], terminals: &[Terminal]) -> Vec<Action> {
    let by_name: HashMap<&str, usize> = actions
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name(), i))
        .collect();
    // The snapshot owner for each key, when one is present in this set.
    let mut owner_of_key: HashMap<anneal_core::Digest, usize> = HashMap::new();
    for (i, a) in actions.iter().enumerate() {
        if a.cache_policy() == CachePolicy::SnapshotBased {
            if let Some(key) = a.snapshot_key() {
                owner_of_key.entry(key).or_insert(i);
            }
        }
    }

    /// Include `index` and everything it transitively demands.
    fn reach(
        index: usize,
        actions: &[Action],
        by_name: &HashMap<&str, usize>,
        owner_of_key: &HashMap<anneal_core::Digest, usize>,
        included: &mut Vec<bool>,
    ) {
        if included[index] {
            return;
        }
        included[index] = true;
        let action = &actions[index];
        // Data edges: this action's declared output references.
        for input in action.inputs().values() {
            if let InputSource::Output {
                action: producer,
                name: _,
            } = &input.source
            {
                if let Some(&p) = by_name.get(producer.as_str()) {
                    reach(p, actions, by_name, owner_of_key, included);
                }
            }
        }
        // Snapshot-owner edge: a consumer demands the action that saves the
        // snapshot it restores, even though no input references it.
        if action.cache_policy() == CachePolicy::SnapshotConsuming {
            if let Some(key) = action.snapshot_key() {
                if let Some(&owner) = owner_of_key.get(&key) {
                    reach(owner, actions, by_name, owner_of_key, included);
                }
            }
        }
    }

    let mut included = vec![false; actions.len()];
    for terminal in terminals {
        if let Some(&index) = by_name.get(terminal.action.as_str()) {
            reach(index, actions, &by_name, &owner_of_key, &mut included);
        }
    }
    actions
        .iter()
        .enumerate()
        .filter(|(i, _)| included[*i])
        .map(|(_, a)| a.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anneal_action::{Action, ActionBuilder};
    use anneal_core::Digest;
    use std::path::PathBuf;

    fn action(name: &str) -> ActionBuilder {
        Action::builder(name.to_owned(), vec!["./tool".to_owned()])
    }

    fn out_dep(builder: ActionBuilder, producer: &str, output: &str) -> ActionBuilder {
        builder.dependency_input(output, format!("{output}.file"), producer, output)
    }

    fn snapshot_pair() -> (Action, Action) {
        let key = Digest::of(b"state-key");
        let owner = action("install")
            .output("modules", "node_modules")
            .snapshot(key, vec![PathBuf::from("node_modules")])
            .build();
        // A script consumer: source inputs only — no reference to `install`.
        // Its node_modules arrives by snapshot restore under the same key.
        let consumer = action("script")
            .source_input("src", "src.ts", Digest::of(b"src"))
            .output(TEST_RESULT_OUTPUT, TEST_RESULT_OUTPUT)
            .snapshot_restore(key, vec![PathBuf::from("node_modules")])
            .build();
        (owner, consumer)
    }

    /// The worked example from the design: a closure of eight actions where
    /// `build` demands four and `test` demands six — and the dependency's
    /// tests are demanded by neither.
    #[test]
    fn build_and_test_demand_sets_on_the_worked_example() {
        let fetch = action("lib fetch").output("crates", "crates.tar").build();
        let lib_compile = out_dep(
            action("lib compile").source_input("src", "lib.rs", Digest::of(b"lib")),
            "lib fetch",
            "crates",
        )
        .output("rlib", "lib.rlib")
        .build();
        let lib_test_compile = out_dep(
            out_dep(
                action("lib test-compile").source_input("src", "lib_test.rs", Digest::of(b"lt")),
                "lib compile",
                "rlib",
            ),
            "lib fetch",
            "crates",
        )
        .output("bin", "lib_test.bin")
        .build();
        let lib_test_run = out_dep(action("lib test-run"), "lib test-compile", "bin")
            .output(TEST_RESULT_OUTPUT, TEST_RESULT_OUTPUT)
            .build();
        let config_gen = action("config gen")
            .source_input("tpl", "template.txt", Digest::of(b"tpl"))
            .output("config", "config.json")
            .build();
        let app_compile = out_dep(
            out_dep(
                out_dep(
                    action("app compile").source_input("src", "main.rs", Digest::of(b"main")),
                    "config gen",
                    "config",
                ),
                "lib compile",
                "rlib",
            ),
            "lib fetch",
            "crates",
        )
        .output("bin", "app.bin")
        .build();
        let app_test_compile = out_dep(
            out_dep(
                action("app test-compile").source_input("src", "app_test.rs", Digest::of(b"at")),
                "app compile",
                "bin",
            ),
            "lib compile",
            "rlib",
        )
        .output("bin", "app_test.bin")
        .build();
        let app_test_run = out_dep(action("app test-run"), "app test-compile", "bin")
            .output(TEST_RESULT_OUTPUT, TEST_RESULT_OUTPUT)
            .build();

        let all = vec![
            fetch,
            lib_compile,
            lib_test_compile,
            lib_test_run,
            config_gen,
            app_compile,
            app_test_compile,
            app_test_run,
        ];
        fn names(set: &[Action]) -> Vec<&str> {
            set.iter().map(|a| a.name()).collect()
        }

        // build //app:app — terminals are app's providers.
        let build = demanded(
            &all,
            &[Terminal {
                action: "app compile".into(),
                output: "bin".into(),
            }],
        );
        assert_eq!(
            names(&build),
            vec!["lib fetch", "lib compile", "config gen", "app compile"],
            "build demands no test actions"
        );

        // test //app:app — providers + app's results.txt.
        let test = demanded(
            &all,
            &[
                Terminal {
                    action: "app compile".into(),
                    output: "bin".into(),
                },
                Terminal {
                    action: "app test-run".into(),
                    output: TEST_RESULT_OUTPUT.into(),
                },
            ],
        );
        assert_eq!(
            names(&test),
            vec![
                "lib fetch",
                "lib compile",
                "config gen",
                "app compile",
                "app test-compile",
                "app test-run",
            ],
            "testing app runs app's tests, never lib's"
        );
    }

    #[test]
    fn snapshot_consumers_demand_their_owner() {
        let (owner, consumer) = snapshot_pair();
        let all = vec![owner, consumer.clone()];
        let set = demanded(
            &all,
            &[Terminal {
                action: "script".into(),
                output: TEST_RESULT_OUTPUT.into(),
            }],
        );
        let names: Vec<&str> = set.iter().map(|a| a.name()).collect();
        assert_eq!(
            names,
            vec!["install", "script"],
            "the walk must cross the snapshot-owner edge the inputs don't express"
        );
    }

    #[test]
    fn unconsumed_unexposed_outputs_are_dead() {
        let dead = action("side effect").output("x", "x.txt").build();
        let needed = out_dep(action("main"), "side effect", "x")
            .output("bin", "app.bin")
            .build();
        // `dead` is consumed here — kept. Now without the consumer:
        // Demanded with no terminals includes nothing (a provider-less request).
        let set = demanded(&[dead.clone(), needed.clone()], &[]);
        assert!(set.is_empty());
        let set = demanded(
            &[dead, needed],
            &[Terminal {
                action: "main".into(),
                output: "bin".into(),
            }],
        );
        assert_eq!(set.len(), 2, "a consumed producer is demanded");
    }

    #[test]
    fn terminals_for_absent_actions_are_ignored() {
        let a = action("a").output("o", "o.txt").build();
        let set = demanded(
            &[a],
            &[Terminal {
                action: "nope".into(),
                output: "o".into(),
            }],
        );
        assert!(set.is_empty());
    }

    #[test]
    fn diamond_edges_do_not_loop_or_duplicate() {
        let base = action("base").output("o", "base.o").build();
        let left = out_dep(action("left"), "base", "o")
            .output("o", "left.o")
            .build();
        let right = out_dep(action("right"), "base", "o")
            .output("o", "right.o")
            .build();
        let top = action("top")
            .dependency_input("l", "l.file", "left", "o")
            .dependency_input("r", "r.file", "right", "o")
            .output("o", "top.o")
            .build();
        let set = demanded(
            &[base, left, right, top],
            &[Terminal {
                action: "top".into(),
                output: "o".into(),
            }],
        );
        assert_eq!(set.len(), 4);
    }
}
