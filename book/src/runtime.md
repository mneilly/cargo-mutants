# Shared runtime builds

`cargo mutants --runtime` enables an experimental execution backend adapted from
[mutagen](https://github.com/llogiq/mutagen). It compiles compatible mutations
together, then starts fresh test processes with one mutation selected at runtime.
The default execution backend still builds each mutation separately.

```sh
cargo mutants --runtime
cargo mutants --runtime --mutagen-operators
```

No annotations, dependencies, or changes to your checked-in manifests are needed.
Instrumentation is confined to scratch copies. Stable Rust is supported;
nightly specialization is not used.

## Execution

After the usual baseline, cargo-mutants groups mutations by their selected test
packages and instruments function bodies. Each alternative contains the same
source change that would be tested by the ordinary backend. The original body
remains available when no mutation is selected.

Each group is compiled once. Before testing mutations, the inactive instrumented
program is tested, unless `--baseline=skip` was requested. Cargo test executables
are discovered from Cargo's JSON messages and executed directly, with their
package directory as the working directory. With `--test-tool=nextest`, the
backend builds reusable Nextest binary metadata and runs Nextest against it.
Each mutation runs in fresh processes; test state is not shared between mutations.

The usual timeout and process-group termination machinery remains active. The
per-mutation test timeout covers all test binaries and doctests together.

Doctests are preserved with Cargo's doctest runner. They may require separate
compilations per mutation, so projects with many doctests can see a smaller
speedup. Existing target-selection options such as `-- --all-targets` can exclude
doctests when appropriate for your project.

## Compatibility and fallback

Rust checks every alternative in a shared build. If a build fails, cargo-mutants
splits the group and retries. A single alternative that still cannot be built or
validated with instrumentation uses the ordinary backend. A failed shared build
is never counted as a caught or unviable mutation; only the eventual experiment's
result is reported.

Mutations in `const fn` and outside function bodies use individual builds.
`--check` also uses individual checks. Unsupported Cargo arguments and explicit
cross-compilation targets retain the ordinary runner. Benchmark targets and custom
library, binary, and example harnesses also retain Cargo's invocation behavior. Nextest invocations with
additional test arguments currently retain the ordinary runner. These fallbacks
preserve the selected mutation set.

`--runtime` cannot be combined with `--in-place`. Parallel workers (`--jobs`)
use independent scratch directories and shared builds.

## Batch size and output

`--runtime-batch-size N` bounds a shared build to at most N mutants; the default
is 64. Smaller batches reduce generated source size and the cost of isolating
incompatible alternatives. Larger batches can save compilations when most
alternatives are compatible. Parallel workers may use smaller batches to divide
work among them.

Existing mutant names, filters, diffs, exit codes, and result classifications are
preserved. Preparation logs are named `mutants.out/log/runtime_NNN.log`. Runtime
mutant logs identify their selection ID and preparation log. Runtime outcomes
contain a test phase; the shared compilation is recorded in the preparation log.
Machine-readable compiler stdout and Nextest's reusable metadata are stored
beside that log. Compiler stdout is captured separately from diagnostics to
preserve complete artifact records.

These settings can also be checked into `.cargo/mutants.toml`:

```toml
runtime = true
runtime_batch_size = 64
mutagen_operators = true
```

Use `--runtime=false` or `--mutagen-operators=false` to override those settings.

## Additional mutation operators

`--mutagen-operators` adds operators ported from mutagen to syn 2:

* Flip boolean literals.
* Increment or decrement integer literals, preserving explicit suffixes. Zero
  has no decrement alternative, and `u128::MAX` has no increment alternative.
  The compiler checks representability in the original context; these source
  replacements do not apply mutagen's runtime wrapping casts.
* Replace an empty string with `"A"`; for a nonempty string, clear it, prepend
  `"-"`, or append `"-"`.
* Delete a semicolon-terminated function or method call.

These operators work with either execution backend and honor the usual filters
and skip/exclusion attributes. They are opt-in, so existing mutation lists do not
change merely by enabling runtime execution. Unsupported replacements are
classified through compilation, rather than an intentional runtime panic.

See `third_party/mutagen/README.md` in the source distribution for the upstream
revision, adapted files, and license information.
