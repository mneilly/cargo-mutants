# Code adapted from mutagen

Upstream: https://github.com/llogiq/mutagen
Revision: a6377c4c3f360afeb7a287c1c17e4b69456d5f53
Authors: Andre Bogus and Samuel Pilz, and the mutagen contributors.

The following code is adapted under upstream's MIT license, reproduced in
LICENSE.MIT:

* `src/runtime/support.rs`: mutation selection from
  `mutagen-core/src/runtime_config.rs`; uses stable `OnceLock`, omits coverage
  recording and mutable global test configuration, and namespaces the selector.
* `src/runtime/mutators.rs`: literal replacement rules and call-statement
  discovery from `mutagen-core/src/mutator/mutator_lit_{bool,int,str}.rs` and
  `mutator_stmt_call.rs`, ported to syn 2 and cargo-mutants' discovery model.
* `src/runtime/runner.rs`: Cargo JSON test artifact discovery from
  `mutagen-runner/src/main.rs::compile_tests` and direct test execution from
  `mutagen-runner/src/test_bin.rs`, integrated with cargo-mutants' process groups,
  timeout handling, logs, and Cargo package working directories.

Runtime alternatives are checked by the stable Rust compiler. Failed shared
builds are split; unsupported single alternatives use the ordinary runner.
Upstream's specialization-based optimistic mutations and panic fallback are
not included.
