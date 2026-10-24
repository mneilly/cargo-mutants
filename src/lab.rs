// Copyright 2021-2025 Martin Pool

//! Successively apply mutations to the source code and run cargo to check,
//! build, and test them.

#![warn(clippy::pedantic)]

use std::cmp::{max, min};
use std::collections::{BTreeMap, HashSet};
use std::panic::resume_unwind;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{thread, vec};

use crate::runtime::{Experiment, RuntimePlan, runner::Invocation};
use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, debug_span, error, trace, warn};

use crate::{
    BaselineStrategy, BuildDir, Console, Context, Mutant, Options, Phase, Result, Scenario,
    ScenarioOutcome, cargo::run_cargo, options::TestPackages, outcome::LabOutcome,
    output::OutputDir, package::Package, package::PackageSelection, timeouts::Timeouts,
    workspace::Workspace,
};

/// Run all possible mutation experiments.
///
/// This is called after all filtering is complete, so all the mutants here will be tested
/// or checked.
///
/// Before testing the mutants, the lab checks that the source tree passes its tests with no
/// mutations applied.
pub fn test_mutants(
    mut mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    console.set_debug_log(output_dir.open_debug_log()?);
    if options.shuffle {
        fastrand::shuffle(&mut mutants);
    }
    output_dir.write_mutants_list(&mutants)?;
    console.discovered_mutants(&mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let output_mutex = Mutex::new(output_dir);
    let baseline_build_dir = BuildDir::for_baseline(workspace, options, console)?;
    let jobserver = options
        .jobserver
        .then(|| {
            let n_tasks = options.jobserver_tasks.unwrap_or_else(num_cpus::get);
            debug!(n_tasks, "starting jobserver");
            jobserver::Client::new(n_tasks)
        })
        .transpose()
        .context("Start jobserver")?;
    let tests_for_mutant = TestsForMutant::new(options, workspace);
    let lab = Lab {
        output_mutex,
        jobserver,
        tests_for_mutant,
        package_environment: workspace.package_environment(),
        options,
        console,
    };
    let timeouts = match options.baseline {
        BaselineStrategy::Run => {
            let outcome = lab.run_baseline(&baseline_build_dir, &mutants)?;
            if outcome.success() {
                Timeouts::from_baseline(&outcome, options)
            } else {
                error!(
                    "cargo {phase} failed in an unmutated tree, so no mutants were tested",
                    phase = outcome.last_phase(),
                );
                return lab
                    .output_mutex
                    .into_inner()
                    .expect("lock output_dir")
                    .finish();
            }
        }
        BaselineStrategy::Skip => Timeouts::without_baseline(options),
    };
    debug!(?timeouts);

    let build_dir_0 = Mutex::new(Some(baseline_build_dir));
    // Create n threads, each dedicated to one build directory. Each of them tries to take a
    // scenario to test off the queue, and then exits when there are no more left.
    console.start_testing_mutants(mutants.len());
    let n_threads = max(1, min(options.jobs.unwrap_or(1), mutants.len()));
    let batch_size = if options.runtime {
        mutants
            .len()
            .div_ceil(n_threads)
            .min(options.runtime_batch_size.max(1))
    } else {
        1
    };
    let work_queue = &Mutex::new(
        mutants
            .into_iter()
            .enumerate()
            .map(|(index, mutant)| (index + 1, mutant))
            .collect_vec()
            .into_iter(),
    );
    thread::scope(|scope| -> crate::Result<()> {
        let mut threads = Vec::new();
        for _i_thread in 0..n_threads {
            threads.push(scope.spawn(|| -> crate::Result<()> {
                trace!(thread_id = ?thread::current().id(), "start thread");
                // First thread to start can use the baseline's build dir;
                // others need to copy a new one
                let build_dir_0 = build_dir_0.lock().expect("lock build dir 0").take(); // separate for lock
                let build_dir = &if let Some(d) = build_dir_0 {
                    d
                } else {
                    BuildDir::copy_from(workspace.root(), options, console)?
                };
                lab.run_queue(build_dir, timeouts, work_queue, batch_size)
            }));
        }
        join_threads(threads)
    })?;

    let output_dir = lab
        .output_mutex
        .into_inner()
        .expect("final unlock mutants queue");
    console.lab_finished(&output_dir.lab_outcome, start_time, options);
    let lab_outcome = output_dir.finish()?;
    if lab_outcome.total_mutants == 0 {
        // This should be unreachable as we also bail out before copying
        // the tree if no mutants are generated.
        warn!("No mutants were generated");
    } else if lab_outcome.unviable == lab_outcome.total_mutants {
        warn!(
            "No mutants were viable: perhaps there is a problem with building in a scratch directory. Look in mutants.out/log/* for more information."
        );
    }
    Ok(lab_outcome)
}

#[mutants::skip] // it's a little hard to observe that the threads were collected?
fn join_threads(threads: Vec<thread::ScopedJoinHandle<'_, Result<()>>>) -> Result<()> {
    // The errors potentially returned from `join` are a special `std::thread::Result`
    // that does not implement error, indicating that the thread panicked.
    // Probably the most useful thing is to `resume_unwind` it.
    // Inside that, there's an actual Mutants error indicating a non-panic error.
    // Most likely, this would be "interrupted" but it might be some IO error
    // etc. In that case, print them all and return the first.
    let errors = threads
        .into_iter()
        .filter_map(|thread| match thread.join() {
            Err(panic) => resume_unwind(panic),
            Ok(Ok(())) => None,
            Ok(Err(err)) => {
                // To avoid console spam don't print "interrupted" errors for each thread,
                // since that should have been printed by check_interrupted but do return them.
                if err.to_string() != "interrupted" {
                    error!("Worker thread failed: {:?}", err);
                }
                Some(err)
            }
        })
        .collect_vec();
    if let Some(first_err) = errors.into_iter().next() {
        Err(first_err)
    } else {
        Ok(())
    }
}

/// Common context across all scenarios, threads, and build dirs.
struct Lab<'a> {
    output_mutex: Mutex<OutputDir>,
    jobserver: Option<jobserver::Client>,
    tests_for_mutant: TestsForMutant,
    package_environment: BTreeMap<camino::Utf8PathBuf, Vec<(String, String)>>,
    options: &'a Options,
    console: &'a Console,
}

impl Lab<'_> {
    /// Run the baseline scenario, which is the same as running `cargo test` on the unmutated
    /// tree.
    ///
    /// If it fails, return None, indicating that no further testing should be done.
    ///
    /// If it succeeds, return the timeouts to be used for the other scenarios.
    fn run_baseline(&self, build_dir: &BuildDir, mutants: &[Mutant]) -> Result<ScenarioOutcome> {
        let all_mutated_packages: Vec<Arc<Package>> = mutants
            .iter()
            .map(|m| Arc::clone(&m.source_file.package))
            .sorted_by_key(|p| p.name.clone())
            .unique()
            .collect_vec();
        self.make_worker(build_dir).run_one_scenario(
            &Scenario::Baseline,
            &PackageSelection::Explicit(all_mutated_packages),
            Timeouts::for_baseline(self.options),
        )
    }

    /// Run until the input queue is empty.
    ///
    /// The queue, inside a mutex, can be consumed by multiple threads.
    fn run_queue(
        &self,
        build_dir: &BuildDir,
        timeouts: Timeouts,
        work_queue: &Mutex<vec::IntoIter<Experiment>>,
        batch_size: usize,
    ) -> Result<()> {
        self.make_worker(build_dir)
            .run_queue(work_queue, timeouts, batch_size)
    }

    fn make_worker<'a>(&'a self, build_dir: &'a BuildDir) -> Worker<'a> {
        Worker {
            build_dir,
            output_mutex: &self.output_mutex,
            jobserver: self.jobserver.as_ref(),
            tests_for_mutant: &self.tests_for_mutant,
            package_environment: &self.package_environment,
            options: self.options,
            console: self.console,
        }
    }
}

/// A worker owns one build directory and runs a single thread of testing.
///
/// It consumes jobs from an input queue and runs them until the queue is empty,
/// appending output to the output directory.
struct Worker<'a> {
    build_dir: &'a BuildDir,
    output_mutex: &'a Mutex<OutputDir>,
    jobserver: Option<&'a jobserver::Client>,
    tests_for_mutant: &'a TestsForMutant,
    package_environment: &'a BTreeMap<camino::Utf8PathBuf, Vec<(String, String)>>,
    options: &'a Options,
    console: &'a Console,
}

impl Worker<'_> {
    /// Take disjoint batches from the queue; each worker owns its build directory.
    fn run_queue(
        mut self,
        work_queue: &Mutex<vec::IntoIter<Experiment>>,
        timeouts: Timeouts,
        batch_size: usize,
    ) -> Result<()> {
        let _span = debug_span!("worker thread", build_dir = ?self.build_dir.path()).entered();
        loop {
            let batch = work_queue
                .lock()
                .expect("Lock pending work queue")
                .by_ref()
                .take(batch_size)
                .collect_vec();
            if batch.is_empty() {
                return Ok(());
            }
            // The default test selection is per mutated package. Build separate
            // groups so a shared build never silently broadens that selection.
            let mut groups = BTreeMap::<String, Vec<Experiment>>::new();
            for experiment in batch {
                let key = if *self.tests_for_mutant == TestsForMutant::Mutated {
                    experiment.1.source_file.package.version_qualified_name()
                } else {
                    String::new()
                };
                groups.entry(key).or_default().push(experiment);
            }
            for group in groups.into_values() {
                self.run_runtime_batch(group, timeouts)?;
            }
        }
    }

    fn test_packages(&self, mutant: &Mutant) -> PackageSelection {
        match self.tests_for_mutant {
            TestsForMutant::Workspace => PackageSelection::All,
            TestsForMutant::Mutated => {
                PackageSelection::Explicit(vec![mutant.source_file.package.clone()])
            }
            TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
        }
    }

    /// Failed preparations are never recorded as mutant outcomes. Split to
    /// isolate incompatible alternatives; singleton failures use the original
    /// runner, which distinguishes compile errors from caught mutations.
    fn run_runtime_batch(
        &mut self,
        experiments: Vec<Experiment>,
        timeouts: Timeouts,
    ) -> Result<()> {
        let test_packages = self.test_packages(&experiments[0].1);
        let invocation = self
            .options
            .runtime
            .then(|| Invocation::new(&test_packages, self.options))
            .flatten();
        let Some(invocation) = invocation else {
            for (_, mutant) in experiments {
                self.run_one_scenario(&Scenario::Mutant(mutant), &test_packages, timeouts)?;
            }
            return Ok(());
        };
        let plan = RuntimePlan::new(&experiments)?;
        let ids: HashSet<_> = plan.experiments.iter().map(|(id, _)| *id).collect();
        let (supported, unsupported): (Vec<_>, Vec<_>) = experiments
            .into_iter()
            .partition(|(id, _)| ids.contains(id));
        if !supported.is_empty() {
            plan.install(self.build_dir)?;
            let result = self.try_runtime_batch(&plan, invocation, &test_packages, timeouts);
            plan.restore(self.build_dir)?;
            if !result? {
                if supported.len() == 1 {
                    let (_, mutant) = supported.into_iter().next().unwrap();
                    self.run_one_scenario(&Scenario::Mutant(mutant), &test_packages, timeouts)?;
                } else {
                    let mut left = supported;
                    let right = left.split_off(left.len() / 2);
                    self.run_runtime_batch(left, timeouts)?;
                    self.run_runtime_batch(right, timeouts)?;
                }
            }
        }
        for (_, mutant) in unsupported {
            self.run_one_scenario(&Scenario::Mutant(mutant), &test_packages, timeouts)?;
        }
        Ok(())
    }

    fn try_runtime_batch(
        &mut self,
        plan: &RuntimePlan,
        invocation: Invocation,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<bool> {
        let mut preparation = self
            .output_mutex
            .lock()
            .expect("lock runtime preparation")
            .start_runtime_build()?;
        debug!(count = plan.experiments.len(), log = %preparation.log_path(), "prepare runtime batch");
        let Some(compiled) = invocation.compile(
            self.build_dir,
            self.jobserver,
            test_packages,
            timeouts.build,
            &mut preparation,
            self.options,
            self.console,
            self.package_environment,
        )?
        else {
            debug!("Shared build failed; split batch or fall back to individual build");
            return Ok(false);
        };
        if self.options.baseline == BaselineStrategy::Run {
            match compiled.run(
                0,
                self.build_dir,
                self.jobserver,
                timeouts.test,
                &mut preparation,
                self.options,
                self.console,
            ) {
                Ok(result) if result.is_success() => {}
                result => {
                    crate::interrupt::check_interrupted()?;
                    preparation.message(&format!(
                        "Inactive instrumented tests failed: {result:?}; falling back"
                    ))?;
                    return Ok(false);
                }
            }
        }
        for (id, mutant) in &plan.experiments {
            let scenario = Scenario::Mutant(mutant.clone());
            let mut output = self
                .output_mutex
                .lock()
                .expect("lock runtime scenario")
                .start_scenario(&scenario)?;
            let dir = self.build_dir.path();
            self.console
                .scenario_started(dir, &scenario, output.open_log_read()?);
            output.write_diff(&mutant.diff(&mutant.mutated_code()))?;
            output.message(&format!(
                "Runtime mutation {id}; shared build: {}",
                preparation.log_path()
            ))?;
            let mut outcome = ScenarioOutcome::new(&output, scenario.clone());
            self.console.scenario_phase_started(dir, Phase::Test);
            let result = compiled.run(
                *id,
                self.build_dir,
                self.jobserver,
                timeouts.test,
                &mut output,
                self.options,
                self.console,
            )?;
            outcome.add_phase_result(result);
            self.console.scenario_phase_finished(dir, Phase::Test);
            self.output_mutex
                .lock()
                .expect("record runtime outcome")
                .add_scenario_outcome(&outcome)?;
            self.console
                .scenario_finished(dir, &scenario, &outcome, self.options);
        }
        Ok(true)
    }

    fn run_one_scenario(
        &mut self,
        scenario: &Scenario,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<ScenarioOutcome> {
        let mut scenario_output = self
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(scenario)?;
        let dir = self.build_dir.path();
        self.console
            .scenario_started(dir, scenario, scenario_output.open_log_read()?);
        debug!(?test_packages);

        if let Some(mutant) = scenario.mutant() {
            let mutated_code = mutant.mutated_code();
            let diff = scenario.mutant().unwrap().diff(&mutated_code);
            scenario_output.write_diff(&diff)?;
            mutant.apply(self.build_dir, &mutated_code)?;
        }

        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        for &phase in self.options.phases() {
            self.console.scenario_phase_started(dir, phase);
            let timeout = match phase {
                Phase::Test => timeouts.test,
                Phase::Build | Phase::Check => timeouts.build,
            };
            match run_cargo(
                self.build_dir,
                self.jobserver,
                test_packages,
                phase,
                timeout,
                &mut scenario_output,
                self.options,
                self.console,
            ) {
                Ok(phase_result) => {
                    let success = phase_result.is_success(); // so we can move it away
                    outcome.add_phase_result(phase_result);
                    self.console.scenario_phase_finished(dir, phase);
                    if !success {
                        break;
                    }
                }
                Err(err) => {
                    error!(?err, ?phase, "scenario execution internal error");
                    // Some unexpected internal error that stops the program.
                    if let Some(mutant) = scenario.mutant() {
                        mutant.revert(self.build_dir)?;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(mutant) = scenario.mutant() {
            mutant.revert(self.build_dir)?;
        }
        self.output_mutex
            .lock()
            .expect("lock output dir to add outcome")
            .add_scenario_outcome(&outcome)?;
        debug!(outcome = ?outcome.summary());
        self.console
            .scenario_finished(dir, scenario, &outcome, self.options);

        Ok(outcome)
    }
}

/// Which packages to test
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestsForMutant {
    /// Test all packages in the workspace
    Workspace,
    /// Test only the package that was mutated
    Mutated,
    /// Test specific packages
    Explicit(Vec<Arc<Package>>),
}

impl TestsForMutant {
    fn new(options: &Options, workspace: &Workspace) -> Self {
        match options.test_package {
            TestPackages::Workspace => TestsForMutant::Workspace,
            TestPackages::Mutated => TestsForMutant::Mutated,
            TestPackages::Named(ref package_names) => {
                TestsForMutant::Explicit(workspace.packages_by_name(package_names))
            }
        }
    }
}
