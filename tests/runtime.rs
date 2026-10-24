// Copyright 2026 Martin Pool

//! End-to-end tests for shared runtime builds and ported mutagen operators.

use std::collections::BTreeMap;
use std::fs::{read_dir, read_to_string, write};

use serde_json::Value;
use tempfile::TempDir;

#[allow(dead_code)]
mod integration_util;
mod util;
fn run() -> assert_cmd::Command {
    let mut command = integration_util::run();
    command.timeout(util::OUTER_TIMEOUT);
    command
}
use util::{copy_of_testdata, outcome_json};

fn outcomes(tree: &TempDir) -> BTreeMap<String, String> {
    outcome_json(tree)["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|outcome| {
            let mutant = outcome["scenario"].get("Mutant")?;
            Some((
                mutant["name"].as_str().unwrap().to_owned(),
                outcome["summary"].as_str().unwrap().to_owned(),
            ))
        })
        .collect()
}

fn log_contents(tree: &TempDir) -> Vec<(String, String)> {
    read_dir(tree.path().join("mutants.out/log"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                read_to_string(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn runtime_factorial_reuses_one_build_and_preserves_outcomes() {
    let classic = copy_of_testdata("factorial");
    let runtime = copy_of_testdata("factorial");
    run()
        .args(["mutants", "-d"])
        .arg(classic.path())
        .assert()
        .code(2);
    run()
        .args(["mutants", "--runtime", "-d"])
        .arg(runtime.path())
        .assert()
        .code(2);
    assert_eq!(outcomes(&classic), outcomes(&runtime));
    let logs = log_contents(&runtime);
    let preparations: Vec<_> = logs
        .iter()
        .filter(|(name, _)| name.starts_with("runtime_") && name.ends_with(".log"))
        .collect();
    assert_eq!(preparations.len(), 1);
    assert_eq!(preparations[0].1.matches("--no-run").count(), 1);
    for outcome in outcome_json(&runtime)["outcomes"].as_array().unwrap() {
        if outcome["scenario"].get("Mutant").is_some() {
            assert_eq!(outcome["phase_results"].as_array().unwrap().len(), 1);
            assert_eq!(outcome["phase_results"][0]["phase"], "Test");
            assert!(
                !outcome["phase_results"][0]["argv"][0]
                    .as_str()
                    .unwrap()
                    .ends_with("cargo")
            );
        }
    }
}

#[test]
fn runtime_splits_incompatible_generics_and_preserves_const_and_mutagen_operator_outcomes() {
    let classic = copy_of_testdata("runtime");
    let runtime = copy_of_testdata("runtime");
    let original = read_to_string(runtime.path().join("src/lib.rs")).unwrap();
    for (tree, extra) in [(&classic, vec![]), (&runtime, vec!["--runtime"])] {
        run()
            .args(["mutants", "--mutagen-operators", "-d"])
            .arg(tree.path())
            .args(extra)
            .assert()
            .success();
    }
    assert_eq!(outcomes(&classic), outcomes(&runtime));
    let summaries = outcomes(&runtime);
    assert!(summaries.values().any(|summary| summary == "Unviable"));
    let logs = log_contents(&runtime);
    assert!(
        logs.iter()
            .filter(|(name, _)| name.starts_with("runtime_") && name.ends_with(".log"))
            .count()
            > 1
    );
    assert!(logs.iter().any(|(_, log)| log.contains("Runtime mutation")));
    assert_eq!(
        read_to_string(runtime.path().join("src/lib.rs")).unwrap(),
        original
    );
}

#[test]
fn runtime_preserves_doctests_that_are_the_only_checks() {
    let tree = copy_of_testdata("runtime");
    write(
        tree.path().join("Cargo.toml"),
        read_to_string(tree.path().join("Cargo.toml"))
            .unwrap()
            .replace("doctest = false", "doctest = true"),
    )
    .unwrap();
    write(tree.path().join("src/lib.rs"), "/// ```\n/// assert_eq!(cargo_mutants_testdata_runtime::value(), 7);\n/// ```\npub fn value() -> u32 { 7 }\n").unwrap();
    run()
        .args(["mutants", "--runtime", "-d"])
        .arg(tree.path())
        .assert()
        .success();
    let result = outcome_json(&tree);
    assert_eq!(result["caught"], 2);
    assert_eq!(result["missed"], 0);
    assert!(
        log_contents(&tree)
            .iter()
            .any(|(_, log)| log.contains("--doc"))
    );
}

#[test]
fn mutagen_operators_honor_skip_and_exclude_attributes() {
    let tree = copy_of_testdata("runtime");
    write(
        tree.path().join("src/lib.rs"),
        r#"
#[cfg_attr(any(), mutants::skip)]
pub fn included() -> bool { true }
#[mutants::skip]
pub fn skipped() -> bool { false }
#[mutants::exclude_re("replace true")]
pub fn excluded() -> bool { true }
"#,
    )
    .unwrap();
    let result = run()
        .args(["mutants", "--mutagen-operators", "--list", "--json", "-d"])
        .arg(tree.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mutants: Value = serde_json::from_slice(&result).unwrap();
    let literals: Vec<_> = mutants
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["genre"] == "Literal")
        .collect();
    assert_eq!(literals.len(), 1);
    assert!(
        literals[0]["name"]
            .as_str()
            .unwrap()
            .contains("replace true")
    );
}

#[test]
fn runtime_requires_copies_and_a_positive_batch_size() {
    run()
        .args(["mutants", "--runtime", "--in-place"])
        .assert()
        .failure();
    run()
        .args(["mutants", "--runtime-batch-size=0"])
        .assert()
        .failure();
}

#[test]
fn runtime_factorial_nextest_reuses_binary_metadata() {
    let tree = copy_of_testdata("factorial");
    run()
        .args(["mutants", "--runtime", "--test-tool=nextest", "-d"])
        .arg(tree.path())
        .assert()
        .code(2);
    let results = outcome_json(&tree);
    assert_eq!(results["caught"], 4);
    assert_eq!(results["missed"], 1);
    let logs = log_contents(&tree);
    assert_eq!(
        logs.iter()
            .filter(|(name, _)| name.starts_with("runtime_") && name.ends_with(".log"))
            .count(),
        1
    );
    for outcome in results["outcomes"].as_array().unwrap() {
        if outcome["scenario"].get("Mutant").is_some() {
            assert_eq!(outcome["phase_results"].as_array().unwrap().len(), 1);
            assert!(
                outcome["phase_results"][0]["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg.as_str().unwrap().starts_with("--binaries-metadata="))
            );
        }
    }
}

#[test]
fn runtime_cross_package_tests_preserve_workspace_selection() {
    for selection in [
        vec!["--test-workspace=true"],
        vec!["--test-package=cargo-mutants-testdata-cross-package-tests-tests"],
    ] {
        let classic = copy_of_testdata("cross_package_tests");
        let runtime = copy_of_testdata("cross_package_tests");
        for (tree, extra) in [(&classic, vec![]), (&runtime, vec!["--runtime"])] {
            run()
                .args(["mutants", "-d"])
                .arg(tree.path())
                .args(&selection)
                .args(extra)
                .assert()
                .success();
        }
        assert_eq!(outcomes(&classic), outcomes(&runtime));
        assert!(
            log_contents(&runtime)
                .iter()
                .any(|(_, log)| log.contains("Runtime mutation"))
        );
    }
}

#[test]
fn runtime_factorial_parallel_workers_and_small_batches_preserve_every_experiment() {
    let classic = copy_of_testdata("factorial");
    let runtime = copy_of_testdata("factorial");
    run()
        .args(["mutants", "-d"])
        .arg(classic.path())
        .assert()
        .code(2);
    run()
        .args([
            "mutants",
            "--runtime",
            "--jobs=2",
            "--runtime-batch-size=2",
            "-d",
        ])
        .arg(runtime.path())
        .assert()
        .code(2);
    assert_eq!(outcomes(&classic), outcomes(&runtime));
    assert_eq!(outcome_json(&runtime)["total_mutants"], 5);
}

#[test]
fn runtime_hang_when_mutated_is_timed_out() {
    let tree = copy_of_testdata("hang_when_mutated");
    run()
        .args([
            "mutants",
            "--runtime",
            "--timeout=0.5",
            "--re=replace should_stop.*with false",
            "-d",
        ])
        .arg(tree.path())
        .assert()
        .code(3);
    let results = outcome_json(&tree);
    assert_eq!(results["timeout"], 1);
    assert_eq!(results["caught"], 0);
    assert_eq!(results["total_mutants"], 1);
}

#[test]
fn runtime_configuration_can_be_overridden_from_the_command_line() {
    let tree = copy_of_testdata("runtime");
    std::fs::create_dir_all(tree.path().join(".cargo")).unwrap();
    write(
        tree.path().join(".cargo/mutants.toml"),
        "runtime = true\nruntime_batch_size = 2\nmutagen_operators = true\n",
    )
    .unwrap();
    run()
        .args([
            "mutants",
            "--runtime=false",
            "--mutagen-operators=false",
            "--check",
            "--re=constant",
            "-d",
        ])
        .arg(tree.path())
        .assert()
        .success();
    assert!(
        log_contents(&tree)
            .iter()
            .all(|(name, _)| !name.starts_with("runtime_"))
    );
    write(
        tree.path().join(".cargo/mutants.toml"),
        "runtime_batch_size = 0\n",
    )
    .unwrap();
    run()
        .args(["mutants", "--list", "-d"])
        .arg(tree.path())
        .assert()
        .failure();
}

#[test]
fn runtime_inactive_validation_falls_back_without_counting_failures_as_caught() {
    let classic = copy_of_testdata("runtime");
    let runtime = copy_of_testdata("runtime");
    for tree in [&classic, &runtime] {
        write(
            tree.path().join("src/lib.rs"),
            "pub fn value() -> u32 { line!() }\n#[test] fn check() { assert_eq!(value(), 1); }\n",
        )
        .unwrap();
    }
    run()
        .args(["mutants", "-d"])
        .arg(classic.path())
        .assert()
        .code(2);
    run()
        .args(["mutants", "--runtime", "-d"])
        .arg(runtime.path())
        .assert()
        .code(2);
    assert_eq!(outcomes(&classic), outcomes(&runtime));
    assert_eq!(outcome_json(&runtime)["caught"], 1);
    assert_eq!(outcome_json(&runtime)["missed"], 1);
}

#[test]
fn runtime_runs_custom_harnesses_and_restores_package_environment() {
    let tree = copy_of_testdata("runtime");
    write(
        tree.path().join("src/lib.rs"),
        "pub fn value() -> u32 { 7 }\n",
    )
    .unwrap();
    let manifest = read_to_string(tree.path().join("Cargo.toml")).unwrap();
    write(
        tree.path().join("Cargo.toml"),
        format!("{manifest}\n[[test]]\nname = \"custom\"\nharness = false\n"),
    )
    .unwrap();
    std::fs::create_dir(tree.path().join("tests")).unwrap();
    write(
        tree.path().join("tests/custom.rs"),
        r#"
fn main() {
    assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(), "cargo-mutants-testdata-runtime");
    assert_eq!(std::env::var("CARGO_PKG_VERSION").unwrap(), "0.1.0");
    assert!(std::path::Path::new("Cargo.toml").exists());
    assert_eq!(cargo_mutants_testdata_runtime::value(), 7);
}
"#,
    )
    .unwrap();
    run()
        .args(["mutants", "--runtime", "-d"])
        .arg(tree.path())
        .assert()
        .success();
    assert_eq!(outcome_json(&tree)["caught"], 2);
    assert!(
        log_contents(&tree)
            .iter()
            .any(|(_, log)| log.contains("Runtime mutation"))
    );
}

#[cfg(unix)]
#[test]
fn runtime_classifies_aborted_test_binaries_as_caught() {
    let tree = copy_of_testdata("runtime");
    write(tree.path().join("src/lib.rs"), "pub fn value() -> bool { true }\n#[test] fn check() { if !value() { std::process::abort(); } }\n").unwrap();
    run()
        .args(["mutants", "--runtime", "-d"])
        .arg(tree.path())
        .assert()
        .success();
    assert_eq!(outcome_json(&tree)["caught"], 1);
    assert_eq!(outcome_json(&tree)["total_mutants"], 1);
}

#[test]
fn runtime_preserves_checks_in_custom_binary_harnesses() {
    let tree = copy_of_testdata("runtime");
    write(
        tree.path().join("src/lib.rs"),
        "pub fn value() -> u32 { 7 }\n",
    )
    .unwrap();
    let manifest = read_to_string(tree.path().join("Cargo.toml")).unwrap();
    write(
        tree.path().join("Cargo.toml"),
        format!(
            "{manifest}\n[[bin]]\nname = \"custom\"\npath = \"src/main.rs\"\nharness = false\n"
        ),
    )
    .unwrap();
    write(
        tree.path().join("src/main.rs"),
        "fn main() { assert_eq!(cargo_mutants_testdata_runtime::value(), 7); }\n",
    )
    .unwrap();
    run()
        .args(["mutants", "--runtime", "--file=src/lib.rs", "-d"])
        .arg(tree.path())
        .assert()
        .success();
    assert_eq!(outcome_json(&tree)["caught"], 2);
    assert_eq!(outcome_json(&tree)["missed"], 0);
}

#[test]
fn mutagen_operators_list_literal_and_call_statement_mutations() {
    let tree = copy_of_testdata("runtime");
    let output = run()
        .args(["mutants", "--mutagen-operators", "--list", "--json", "-d"])
        .arg(tree.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mutants: Value = serde_json::from_slice(&output).unwrap();
    let mutants = mutants.as_array().unwrap();
    for replacement in ["false", "8", "6", "\"-hello\"", "\"hello-\"", "\"\""] {
        assert!(
            mutants
                .iter()
                .any(|mutant| mutant["genre"] == "Literal" && mutant["replacement"] == replacement),
            "missing literal replacement {replacement}"
        );
    }
    assert!(
        mutants
            .iter()
            .any(|mutant| mutant["genre"] == "CallStatement"
                && mutant["replacement"] == ""
                && mutant["name"].as_str().unwrap().contains("values.push(7)"))
    );
}
