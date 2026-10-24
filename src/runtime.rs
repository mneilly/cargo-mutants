// Copyright 2026 Martin Pool

//! Stable Rust instrumentation and execution of shared mutation builds.
//!
//! Every alternative is a complete function body checked by rustc. This avoids
//! optimistic trait specialization and preserves the source mutation's semantics.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::ops::Range;

use anyhow::Context;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::Result;
use crate::build_dir::BuildDir;
use crate::mutant::Mutant;
use crate::source::SourceFile;
use crate::span::{LineColumn, Span};

pub mod mutators;
pub mod runner;
#[cfg(test)]
mod support;

pub const SELECTOR_ENV: &str = "CARGO_MUTANTS_ACTIVE_MUTANT";
const SUPPORT: &str = include_str!("runtime/support.rs");
type Position = (usize, usize);
type SpanKey = (Position, Position);

/// An ID is allocated centrally before workers consume the queue.
pub type Experiment = (usize, Mutant);

/// Instrumented files and the experiments that they support.
pub struct RuntimePlan {
    pub experiments: Vec<Experiment>,
    files: Vec<(SourceFile, String)>,
}

struct Body {
    range: Range<usize>,
    experiments: Vec<Experiment>,
}

impl RuntimePlan {
    pub fn new(experiments: &[Experiment]) -> Result<Self> {
        let mut by_file = BTreeMap::<_, Vec<_>>::new();
        for experiment in experiments {
            by_file
                .entry(experiment.1.source_file.tree_relative_path.clone())
                .or_default()
                .push(experiment.clone());
        }
        let mut files = Vec::new();
        let mut supported = Vec::new();
        for file_experiments in by_file.into_values() {
            let source_file = file_experiments[0].1.source_file.clone();
            let code = source_file.code();
            let ast = syn::parse_file(code).context("parse file for runtime instrumentation")?;
            let mut visitor = FunctionBodies::default();
            visitor.visit_file(&ast);
            let mut bodies = BTreeMap::<SpanKey, Body>::new();
            for experiment in file_experiments {
                let Some(function) = &experiment.1.function else {
                    continue;
                };
                let Some(span) = visitor.bodies.get(&span_key(function.span)) else {
                    continue;
                };
                let range = byte_range(code, *span);
                let mutation_range = byte_range(code, experiment.1.span);
                if mutation_range.start < range.start || mutation_range.end > range.end {
                    continue;
                }
                bodies
                    .entry(span_key(*span))
                    .or_insert_with(|| Body {
                        range,
                        experiments: Vec::new(),
                    })
                    .experiments
                    .push(experiment.clone());
                supported.push(experiment);
            }
            if !bodies.is_empty() {
                let bodies: Vec<_> = bodies.into_values().collect();
                let instrumented = render_region(code, 0..code.len(), &bodies);
                files.push((source_file, instrumented));
            }
        }
        supported.sort_by_key(|(id, _)| *id);
        Ok(Self {
            experiments: supported,
            files,
        })
    }

    pub fn install(&self, build_dir: &BuildDir) -> Result<()> {
        for (source_file, code) in &self.files {
            build_dir.overwrite_file(&source_file.tree_relative_path, code)?;
        }
        Ok(())
    }

    pub fn restore(&self, build_dir: &BuildDir) -> Result<()> {
        for (source_file, _) in &self.files {
            build_dir.overwrite_file(&source_file.tree_relative_path, source_file.code())?;
        }
        Ok(())
    }
}

/// Render disjoint outer bodies, recursively instrumenting nested functions in
/// the original branch. Only one mutant is active, so mutated branches retain
/// the original definitions of nested functions.
fn render_region(code: &str, region: Range<usize>, bodies: &[Body]) -> String {
    let mut result = String::new();
    let mut cursor = region.start;
    let mut index = 0;
    while index < bodies.len() {
        let body = &bodies[index];
        if body.range.start < region.start || body.range.end > region.end {
            index += 1;
            continue;
        }
        result.push_str(&code[cursor..body.range.start]);
        let mut next = index + 1;
        while next < bodies.len() && bodies[next].range.start < body.range.end {
            next += 1;
        }
        let original = render_region(code, body.range.clone(), &bodies[index + 1..next]);
        result.push_str("{ #[allow(dead_code)] mod __cargo_mutants_runtime {\n");
        result.push_str(SUPPORT);
        result.push_str("\n} match __cargo_mutants_runtime::MutagenRuntimeConfig::get_default().mutation_id() {\n");
        for (id, mutant) in &body.experiments {
            let mut variant = code[body.range.clone()].to_owned();
            let range = byte_range(code, mutant.span);
            variant.replace_range(
                range.start - body.range.start..range.end - body.range.start,
                &format!("{} {}", mutant.replacement, crate::MUTATION_MARKER_COMMENT),
            );
            writeln!(result, "::std::option::Option::Some({id}) => {variant},").unwrap();
        }
        write!(result, "_ => {original},\n}} }}").unwrap();
        cursor = body.range.end;
        index = next;
    }
    result.push_str(&code[cursor..region.end]);
    result
}

#[derive(Default)]
struct FunctionBodies {
    bodies: BTreeMap<SpanKey, Span>,
}

impl<'ast> Visit<'ast> for FunctionBodies {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if i.sig.constness.is_none() && i.sig.unsafety.is_none() {
            self.bodies
                .insert(span_key(i.span().into()), i.block.span().into());
        }
        syn::visit::visit_item_fn(self, i);
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if i.sig.constness.is_none() && i.sig.unsafety.is_none() {
            self.bodies
                .insert(span_key(i.span().into()), i.block.span().into());
        }
        syn::visit::visit_impl_item_fn(self, i);
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if let Some(block) = &i.default
            && i.sig.constness.is_none()
            && i.sig.unsafety.is_none()
        {
            self.bodies
                .insert(span_key(i.span().into()), block.span().into());
        }
        syn::visit::visit_trait_item_fn(self, i);
    }
}

fn span_key(span: Span) -> SpanKey {
    (position(span.start), position(span.end))
}

fn position(line_column: LineColumn) -> Position {
    (line_column.line, line_column.column)
}

fn byte_range(code: &str, span: Span) -> Range<usize> {
    fn offset(code: &str, target: LineColumn) -> usize {
        let mut current = LineColumn { line: 1, column: 1 };
        for (offset, ch) in code.char_indices() {
            if current == target {
                return offset;
            }
            match ch {
                '\n' => {
                    current.line += 1;
                    current.column = 1;
                }
                '\r' => {}
                _ => current.column += 1,
            }
        }
        assert_eq!(current, target, "source coordinate is in bounds");
        code.len()
    }
    offset(code, span.start)..offset(code, span.end)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{Options, visit::mutate_source_str};

    fn plan(code: &str) -> RuntimePlan {
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let experiments = mutants
            .into_iter()
            .enumerate()
            .map(|(i, m)| (i + 1, m))
            .collect::<Vec<_>>();
        RuntimePlan::new(&experiments).unwrap()
    }

    #[test]
    fn runtime_plan_preserves_nested_functions_unicode_and_original_source() {
        let plan =
            plan("fn outer() -> u32 { fn inner() -> u32 { let café = 42; café } inner() + 1 }");
        assert_ne!(plan.experiments, []);
        let code = &plan.files[0].1;
        syn::parse_file(code).unwrap();
        assert!(code.contains("let café = 42; café"));
        for (id, _) in &plan.experiments {
            assert!(code.contains(&format!("Some({id})")));
        }
        assert!(
            plan.experiments
                .iter()
                .any(|(_, m)| m.function.as_ref().unwrap().function_name == "outer::inner")
        );
    }

    #[test]
    fn runtime_plan_leaves_const_functions_and_constants_to_individual_builds() {
        let plan = plan("const VALUE: u32 = 1 + 2; const fn value() -> u32 { 42 }");
        assert_eq!(plan.experiments, []);
        assert_eq!(plan.files, []);
    }

    #[test]
    fn runtime_plan_instruments_methods_and_trait_default_bodies() {
        let plan = plan(
            "struct S; impl S { fn value(&self) -> u32 { 42 } } trait T { fn value(&self) -> u32 { 7 } }",
        );
        assert_eq!(plan.experiments.len(), 4);
        syn::parse_file(&plan.files[0].1).unwrap();
    }

    #[test]
    fn runtime_configuration_selects_exactly_one_mutation() {
        use super::support::MutagenRuntimeConfig;
        assert_eq!(MutagenRuntimeConfig::from_id(0).mutation_id(), None);
        assert_eq!(MutagenRuntimeConfig::from_id(42).mutation_id(), Some(42));
        // The embedded configuration must also compile and initialize in host tests.
        let _ = MutagenRuntimeConfig::get_default();
    }
}
