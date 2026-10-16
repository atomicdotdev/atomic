//! A token-level workspace lint: comments and substrings are not vocabulary
//! uses. Retired exact string literals belong only in the migration inventory.
use atomic_core::kg::legacy::LEGACY_TERMS;
use proc_macro2::{TokenStream, TokenTree};
use std::{collections::BTreeSet, path::Path};

fn literals(
    tokens: TokenStream,
    forbidden: &BTreeSet<&str>,
    path: &Path,
    failures: &mut Vec<String>,
) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => literals(group.stream(), forbidden, path, failures),
            TokenTree::Literal(literal) => {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    if forbidden.contains(value.value().as_str()) {
                        failures.push(format!("{}: {:?}", path.display(), value.value()));
                    }
                }
            }
            _ => {}
        }
    }
}

fn scan(path: &Path, forbidden: &BTreeSet<&str>, failures: &mut Vec<String>) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            scan(&entry.unwrap().path(), forbidden, failures);
        }
    } else if path.extension().is_some_and(|extension| extension == "rs")
        && !path.ends_with("src/kg/legacy.rs")
    {
        let source = std::fs::read_to_string(path).unwrap();
        let tokens = source
            .parse()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        literals(tokens, forbidden, path, failures);
    }
}

#[test]
fn workspace_has_no_retired_kg_vocabulary_literals() {
    let forbidden = LEGACY_TERMS
        .iter()
        .map(|(term, _)| *term)
        .filter(|term| {
            (term.contains(':') || term.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                && ![
                    "prov:wasDerivedFrom",
                    "prov:used",
                    "prov:wasInformedBy",
                    "prov:generated",
                ]
                .contains(term)
                && !term.starts_with("prefix::")
        })
        .collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() && (name.starts_with("atomic-") || name == "libatomic") {
            scan(&path.join("src"), &forbidden, &mut failures);
            scan(&path.join("tests"), &forbidden, &mut failures);
        }
    }
    assert!(
        failures.is_empty(),
        "retired vocabulary:\n{}",
        failures.join("\n")
    );
}
