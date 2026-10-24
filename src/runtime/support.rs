// Adapted from mutagen-core/src/runtime_config.rs, revision
// a6377c4c3f360afeb7a287c1c17e4b69456d5f53. See third_party/mutagen/LICENSE.MIT.
// This file is also embedded into instrumented function bodies.
/*
MIT License

Copyright (c) 2018

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use std::sync::OnceLock;

pub enum MutagenRuntimeConfig {
    Pass,
    Mutation(usize),
}

impl MutagenRuntimeConfig {
    pub fn get_default() -> &'static Self {
        static RUNTIME_CONFIG: OnceLock<MutagenRuntimeConfig> = OnceLock::new();
        RUNTIME_CONFIG.get_or_init(|| {
            Self::from_id(
                std::env::var("CARGO_MUTANTS_ACTIVE_MUTANT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            )
        })
    }

    pub fn from_id(mutation_id: usize) -> Self {
        if mutation_id == 0 {
            Self::Pass
        } else {
            Self::Mutation(mutation_id)
        }
    }

    pub fn mutation_id(&self) -> Option<usize> {
        if let Self::Mutation(mutation_id) = self {
            Some(*mutation_id)
        } else {
            None
        }
    }
}
