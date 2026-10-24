// Adapted from mutagen-core's literal and statement mutators. See
// third_party/mutagen/README.md and LICENSE.MIT for provenance and license.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};

/// Mutagen's literal mutation rules, expressed as source replacements.
pub fn literal_replacements(lit: &syn::Lit) -> Vec<TokenStream> {
    match lit {
        syn::Lit::Bool(lit) => {
            let replacement = !lit.value;
            vec![quote!(#replacement)]
        }
        syn::Lit::Int(lit) => {
            let Ok(value) = lit.base10_parse::<u128>() else {
                return Vec::new();
            };
            // Preserve the suffix and let Rust check representability. Unlike
            // mutagen's runtime integer casts, invalid boundary values do not wrap.
            [value.checked_add(1), value.checked_sub(1)]
                .into_iter()
                .flatten()
                .map(|value| {
                    syn::LitInt::new(&format!("{value}{}", lit.suffix()), lit.span())
                        .into_token_stream()
                })
                .collect()
        }
        syn::Lit::Str(lit) => {
            let value = lit.value();
            let replacements = if value.is_empty() {
                vec!["A".to_owned()]
            } else {
                vec![String::new(), format!("-{value}"), format!("{value}-")]
            };
            replacements
                .into_iter()
                .map(|value| syn::LitStr::new(&value, lit.span()).into_token_stream())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// syn 2 represents semicolon statements with `Stmt::Expr(_, Some(_))`.
pub fn is_call_statement(stmt: &syn::Stmt) -> bool {
    matches!(
        stmt,
        syn::Stmt::Expr(syn::Expr::Call(_) | syn::Expr::MethodCall(_), Some(_))
    )
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn literal_replacements_preserve_integer_suffixes_and_boundaries() {
        let replacements = literal_replacements(&syn::parse_str("0u8").unwrap());
        assert_eq!(
            replacements
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["1u8"]
        );
        let replacements = literal_replacements(
            &syn::parse_str("340282366920938463463374607431768211455u128").unwrap(),
        );
        assert_eq!(replacements.len(), 1);
    }

    #[test]
    fn literal_replacements_follow_mutagen_string_rules() {
        for (input, expected) in [
            ("\"\"", vec!["\"A\""]),
            ("\"é\"", vec!["\"\"", "\"-é\"", "\"é-\""]),
        ] {
            let replacements = literal_replacements(&syn::parse_str(input).unwrap());
            assert_eq!(
                replacements
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}
