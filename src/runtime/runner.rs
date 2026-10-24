// Adapted from mutagen-runner's compile_tests and TestBin. See
// third_party/mutagen/README.md and LICENSE.MIT for provenance and license.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use anyhow::Context;
use camino::Utf8PathBuf;
use cargo_metadata::{Artifact, Message, TargetKind};

use crate::cargo::{cargo_argv, run_cargo_command};
use crate::options::TestTool;
use crate::outcome::{Phase, PhaseResult};
use crate::output::ScenarioOutput;
use crate::package::PackageSelection;
use crate::process::{Exit, Process};
use crate::{BuildDir, Console, Options, Result};

/// Cargo arguments separated from arguments passed to the test harness.
pub struct Invocation {
    build_argv: Vec<String>,
    test_args: Vec<String>,
    doc_argv: Option<Vec<String>>,
    nextest: bool,
    no_fail_fast: bool,
}

impl Invocation {
    /// Unsupported invocations retain the ordinary Cargo runner's behavior.
    pub fn new(packages: &PackageSelection, options: &Options) -> Option<Self> {
        if options.check_only {
            return None;
        }
        if options.test_tool() == TestTool::Nextest {
            // Nextest itself restores package environments, target runners and
            // dynamic library paths from its reusable binary metadata.
            if !options.additional_cargo_test_args.is_empty() {
                return None;
            }
            let mut build_argv = cargo_argv(packages, Phase::Build, options);
            "list".clone_into(&mut build_argv[2]);
            build_argv.retain(|arg| arg != "--no-run");
            build_argv.extend([
                "--message-format=json".to_owned(),
                "--list-type=binaries-only".to_owned(),
            ]);
            return Some(Self {
                build_argv,
                test_args: Vec::new(),
                doc_argv: None,
                nextest: true,
                no_fail_fast: false,
            });
        }
        let argv = cargo_argv(packages, Phase::Test, options);
        let delimiter = argv
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(argv.len());
        let mut build_argv = argv[..delimiter].to_vec();
        let mut test_args = argv.get(delimiter + 1..).unwrap_or_default().to_vec();
        let mut doctests = true;
        let mut value_next = false;
        for arg in &argv[2..delimiter] {
            if value_next {
                value_next = false;
                continue;
            }
            let name = arg.split('=').next().unwrap();
            match name {
                // Cross compilation and explicit runners need Cargo to launch tests.
                "--target" | "--doc" | "--no-run" | "--message-format" | "-Z" => return None,
                "--lib" | "--bins" | "--tests" | "--examples" | "--benches" | "--all-targets" => {
                    doctests = false;
                }
                "--bin" | "--test" | "--example" | "--bench" => {
                    doctests = false;
                    value_next = !arg.contains('=');
                }
                "--package" | "-p" | "--exclude" | "--features" | "-F" | "--jobs" | "-j"
                | "--profile" | "--target-dir" | "--manifest-path" | "--config" | "--color" => {
                    value_next = !arg.contains('=');
                }
                "--workspace"
                | "--all"
                | "--all-features"
                | "--no-default-features"
                | "--verbose"
                | "-v"
                | "-vv"
                | "--quiet"
                | "-q"
                | "--release"
                | "-r"
                | "--locked"
                | "--offline"
                | "--frozen"
                | "--no-fail-fast"
                | "--keep-going" => {}
                _ if !arg.starts_with('-') => {
                    // Cargo accepts a single positional test name filter.
                    test_args.insert(0, arg.clone());
                }
                _ => return None,
            }
        }
        if value_next || std::env::var_os("CARGO_BUILD_TARGET").is_some() {
            return None;
        }
        let doc_argv = doctests.then(|| {
            let mut doc_command = argv.clone();
            doc_command.insert(delimiter, "--doc".to_owned());
            doc_command
        });
        build_argv.extend(["--no-run".to_owned(), "--message-format=json".to_owned()]);
        Some(Self {
            build_argv,
            test_args,
            doc_argv,
            nextest: false,
            no_fail_fast: argv.iter().any(|arg| arg == "--no-fail-fast"),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compile(
        self,
        build_dir: &BuildDir,
        jobserver: Option<&jobserver::Client>,
        packages: &PackageSelection,
        timeout: Option<Duration>,
        output: &mut ScenarioOutput,
        options: &Options,
        console: &Console,
        package_environment: &BTreeMap<Utf8PathBuf, Vec<(String, String)>>,
    ) -> Result<Option<CompiledTests>> {
        let stdout = output.open_stdout()?;
        output.message(&format!("Compiler stdout: {}", output.stdout_path()))?;
        let result = run_cargo_command(
            build_dir,
            jobserver,
            self.build_argv,
            Phase::Build,
            timeout,
            output,
            options,
            console,
            0,
            Some(stdout),
        )?;
        if !result.is_success() {
            return Ok(None);
        }
        if self.nextest {
            let reader = BufReader::new(File::open(output.stdout_path())?);
            for line in reader.lines() {
                let line = line?;
                if serde_json::from_str::<nextest_metadata::BinaryListSummary>(&line).is_ok() {
                    let metadata_path = output
                        .output_dir
                        .join(output.log_path())
                        .with_extension("json");
                    std::fs::write(&metadata_path, line)
                        .context("write reusable Nextest binary metadata")?;
                    return Ok(Some(CompiledTests {
                        binaries: Vec::new(),
                        test_args: Vec::new(),
                        doc_argv: None,
                        library_dirs: BTreeSet::new(),
                        no_fail_fast: false,
                        package_environment: BTreeMap::new(),
                        nextest_argv: Some(vec![
                            crate::cargo::cargo_bin(),
                            "nextest".to_owned(),
                            "run".to_owned(),
                            format!("--binaries-metadata={metadata_path}"),
                        ]),
                    }));
                }
            }
            output.message("No Nextest binary metadata; using the ordinary runner")?;
            return Ok(None);
        }
        let mut binaries = Vec::new();
        let mut has_doctests = false;
        let mut library_dirs = BTreeSet::new();
        let reader = BufReader::new(File::open(output.stdout_path())?);
        for line in reader.lines() {
            let line = line?;
            let Ok(Message::CompilerArtifact(artifact)) = serde_json::from_str::<Message>(&line)
            else {
                continue;
            };
            for filename in &artifact.filenames {
                if let Some(parent) = filename.parent() {
                    library_dirs.insert(parent.to_owned());
                }
            }
            let selected = match packages {
                PackageSelection::All => artifact.manifest_path.starts_with(build_dir.path()),
                PackageSelection::Explicit(packages) => packages.iter().any(|p| {
                    artifact.manifest_path
                        == build_dir.path().join(&p.relative_dir).join("Cargo.toml")
                }),
            };
            if !selected {
                continue;
            }
            if artifact.target.kind.contains(&TargetKind::Bench)
                || (artifact.executable.is_some()
                    && !artifact.profile.test
                    && !artifact.target.kind.contains(&TargetKind::Test)
                    && has_custom_harness(&artifact)?)
            {
                output.message(
                    "Target requires Cargo's harness invocation; using individual builds",
                )?;
                return Ok(None);
            }
            has_doctests |= artifact.target.doctest
                && artifact.target.kind.iter().any(|kind| {
                    matches!(
                        kind,
                        TargetKind::Lib | TargetKind::RLib | TargetKind::ProcMacro
                    )
                });
            if (artifact.profile.test || artifact.target.kind.contains(&TargetKind::Test))
                && artifact.executable.is_some()
            {
                binaries.push(artifact);
            }
        }
        binaries.sort_by(|left, right| left.executable.cmp(&right.executable));
        binaries.dedup_by(|left, right| left.executable == right.executable);
        if binaries.is_empty() && !(has_doctests && self.doc_argv.is_some()) {
            output.message("No runnable test artifacts; using the ordinary runner")?;
            return Ok(None);
        }
        // Cargo normally adds this directory for tests that dynamically link libstd.
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        if let Ok(output) = std::process::Command::new(rustc)
            .args(["--print", "target-libdir"])
            .current_dir(build_dir.path())
            .output()
            && output.status.success()
            && let Ok(path) = std::str::from_utf8(&output.stdout)
        {
            library_dirs.insert(Utf8PathBuf::from(path.trim()));
        }
        Ok(Some(CompiledTests {
            binaries,
            test_args: self.test_args,
            doc_argv: self.doc_argv.filter(|_| has_doctests),
            library_dirs,
            nextest_argv: None,
            no_fail_fast: self.no_fail_fast,
            package_environment: package_environment.clone(),
        }))
    }
}

pub struct CompiledTests {
    binaries: Vec<Artifact>,
    test_args: Vec<String>,
    doc_argv: Option<Vec<String>>,
    library_dirs: BTreeSet<Utf8PathBuf>,
    nextest_argv: Option<Vec<String>>,
    no_fail_fast: bool,
    package_environment: BTreeMap<Utf8PathBuf, Vec<(String, String)>>,
}

impl CompiledTests {
    /// Execute fresh processes against the same artifacts. The timeout applies to
    /// the whole scenario, including all binaries and any doctests.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        active_mutant: usize,
        build_dir: &BuildDir,
        jobserver: Option<&jobserver::Client>,
        timeout: Option<Duration>,
        output: &mut ScenarioOutput,
        options: &Options,
        console: &Console,
    ) -> Result<PhaseResult> {
        if let Some(argv) = &self.nextest_argv {
            return run_cargo_command(
                build_dir,
                jobserver,
                argv.clone(),
                Phase::Test,
                timeout,
                output,
                options,
                console,
                active_mutant,
                None,
            );
        }
        let start = Instant::now();
        let mut status = Exit::Success;
        let mut last_argv = Vec::new();
        let mut env = vec![
            (super::SELECTOR_ENV.to_owned(), active_mutant.to_string()),
            ("INSTA_UPDATE".to_owned(), "no".to_owned()),
            ("INSTA_FORCE_PASS".to_owned(), "0".to_owned()),
        ];
        let library_var = if cfg!(windows) {
            "PATH"
        } else if cfg!(target_os = "macos") {
            "DYLD_FALLBACK_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let inherited = std::env::var_os(library_var).unwrap_or_default();
        let paths = self
            .library_dirs
            .iter()
            .map(|p| p.as_std_path().to_owned())
            .chain(std::env::split_paths(&inherited));
        env.push((
            library_var.to_owned(),
            std::env::join_paths(paths)
                .context("join runtime library paths")?
                .into_string()
                .map_err(|_| anyhow::anyhow!("runtime library paths are not UTF-8"))?,
        ));
        for artifact in &self.binaries {
            let remaining = timeout.map(|limit| limit.saturating_sub(start.elapsed()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                status = Exit::Timeout;
                break;
            }
            let cwd = artifact
                .manifest_path
                .parent()
                .context("test artifact has no package directory")?;
            let mut bin_env = env.clone();
            if let Ok(relative_dir) = cwd.strip_prefix(build_dir.path())
                && let Some(values) = self.package_environment.get(relative_dir)
            {
                bin_env.extend(values.iter().cloned());
            }
            bin_env.extend([
                ("CARGO_MANIFEST_DIR".to_owned(), cwd.to_string()),
                (
                    "CARGO_MANIFEST_PATH".to_owned(),
                    artifact.manifest_path.to_string(),
                ),
            ]);
            let argv = std::iter::once(artifact.executable.as_ref().unwrap().to_string())
                .chain(self.test_args.iter().cloned())
                .collect::<Vec<_>>();
            let mut binary_status = Process::run(
                &argv, &bin_env, cwd, remaining, jobserver, output, console, None,
            )?;
            // Cargo converts a test binary killed by a signal into a failing
            // test command. Preserve that classification with direct execution.
            #[cfg(unix)]
            if let Exit::Signalled(signal) = binary_status {
                binary_status = Exit::Failure(128 + signal);
            }
            if !binary_status.is_success() {
                status = binary_status;
            }
            last_argv = argv;
            if status.is_timeout() || (!self.no_fail_fast && !status.is_success()) {
                break;
            }
        }
        if (status.is_success() || (self.no_fail_fast && !status.is_timeout()))
            && let Some(argv) = &self.doc_argv
        {
            let remaining = timeout.map(|limit| limit.saturating_sub(start.elapsed()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                status = Exit::Timeout;
            } else {
                let result = run_cargo_command(
                    build_dir,
                    jobserver,
                    argv.clone(),
                    Phase::Test,
                    remaining,
                    output,
                    options,
                    console,
                    active_mutant,
                    None,
                )?;
                if !result.process_status.is_success() {
                    status = result.process_status;
                }
                last_argv = result.argv;
            }
        }
        // Keep the execution mode explicit in each mutant's log.
        output.message(&format!("Shared runtime test result: {status:?}"))?;
        Ok(PhaseResult {
            phase: Phase::Test,
            duration: start.elapsed(),
            process_status: status,
            argv: last_argv,
        })
    }
}

/// Cargo gives custom binary, library, and example harnesses target-specific
/// arguments. Leave those invocations to Cargo instead of silently missing them.
fn has_custom_harness(artifact: &Artifact) -> Result<bool> {
    let text =
        std::fs::read_to_string(&artifact.manifest_path).context("read test target manifest")?;
    let manifest: toml::Value = text.parse().context("parse test target manifest")?;
    for kind in &artifact.target.kind {
        let key = match kind {
            TargetKind::Bin => "bin",
            TargetKind::Example => "example",
            TargetKind::Lib | TargetKind::RLib => "lib",
            _ => continue,
        };
        let Some(targets) = manifest.get(key) else {
            continue;
        };
        let is_custom = |target: &toml::Value| {
            target.get("harness").and_then(toml::Value::as_bool) == Some(false)
                && target
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .is_none_or(|name| name == artifact.target.name)
        };
        if targets.as_array().map_or_else(
            || is_custom(targets),
            |targets| targets.iter().any(is_custom),
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Args;
    use clap::Parser;

    #[test]
    fn runtime_invocation_preserves_cargo_filters_and_harness_arguments() {
        let args = Args::try_parse_from(["mutants", "--", "--lib", "chosen", "--", "--nocapture"])
            .unwrap();
        let options = Options::from_args(&args).unwrap();
        let invocation = Invocation::new(&PackageSelection::All, &options).unwrap();
        assert_eq!(invocation.test_args, ["chosen", "--nocapture"]);
        assert!(invocation.build_argv.contains(&"--lib".to_owned()));
        assert!(invocation.doc_argv.is_none());
    }

    #[test]
    fn runtime_invocation_leaves_cross_compilation_and_explicit_doctests_to_cargo() {
        for extra in [vec!["--target=x86_64-unknown-linux-gnu"], vec!["--doc"]] {
            let options = Options {
                additional_cargo_test_args: extra.into_iter().map(str::to_owned).collect(),
                ..Options::default()
            };
            assert!(Invocation::new(&PackageSelection::All, &options).is_none());
        }
    }
}
