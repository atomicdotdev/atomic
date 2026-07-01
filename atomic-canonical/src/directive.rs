//! Container/leaf directive parser.
//!
//! The body's typed structure is explicit and local: the lift reads a node's
//! type off the directive name, never off heading position (the fragile
//! "positional lift" the doc rejects). M0 supports:
//!   - container: `:::name{#id key=value ...}` … prose … `:::`
//!   - leaf:      `::name{key=value ...}`  (edges only, no body)
//!
//! Inline directives are deferred (not needed for the Intent slice).

use std::collections::BTreeMap;

use crate::error::{CanonicalError, Result};
use crate::vocab;

/// A parsed directive block.
#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub name: String,
    pub id: Option<String>,
    pub attrs: BTreeMap<String, String>,
    /// Prose inside a container directive (trimmed). Empty for leaf directives.
    pub body: String,
    /// Leaf directives nested inside this container (e.g. `::file-ref`).
    pub children: Vec<Directive>,
}

impl Directive {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(|s| s.as_str())
    }
}

/// Parse all top-level directives out of a markdown body. Prose outside any
/// directive is ignored by the lift (it stays as unlifted narrative).
pub fn parse(body: &str) -> Result<Vec<Directive>> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(rest) = trimmed.strip_prefix(":::") {
            // Container open (a bare "):::" close is handled inside).
            let (name, attrs_src) = split_name_attrs(rest);
            if name.is_empty() {
                // stray close or empty — skip
                i += 1;
                continue;
            }
            check_known(&name)?;
            let (id, attrs) = parse_attrs(attrs_src)?;
            // Gather body lines until a line that is exactly ":::".
            let mut body_lines: Vec<&str> = Vec::new();
            let mut children = Vec::new();
            i += 1;
            let mut closed = false;
            while i < lines.len() {
                let lt = lines[i].trim();
                if lt == ":::" {
                    closed = true;
                    i += 1;
                    break;
                }
                // A leaf directive nested in the container body.
                if lt.starts_with("::") && !lt.starts_with(":::") {
                    if let Some(leaf) = parse_leaf(lt)? {
                        children.push(leaf);
                        i += 1;
                        continue;
                    }
                }
                body_lines.push(lines[i]);
                i += 1;
            }
            if !closed {
                return Err(CanonicalError::Directive(format!(
                    "unterminated container directive ':::{name}' (missing closing ':::')"
                )));
            }
            out.push(Directive {
                name,
                id,
                attrs,
                body: body_lines.join("\n").trim().to_string(),
                children,
            });
        } else if trimmed.starts_with("::") {
            if let Some(leaf) = parse_leaf(trimmed)? {
                out.push(leaf);
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

fn parse_leaf(line: &str) -> Result<Option<Directive>> {
    let rest = match line.trim().strip_prefix("::") {
        Some(r) => r,
        None => return Ok(None),
    };
    let (name, attrs_src) = split_name_attrs(rest);
    if name.is_empty() {
        return Ok(None);
    }
    check_known(&name)?;
    let (id, attrs) = parse_attrs(attrs_src)?;
    Ok(Some(Directive {
        name,
        id,
        attrs,
        body: String::new(),
        children: Vec::new(),
    }))
}

fn check_known(name: &str) -> Result<()> {
    if !vocab::is_known_directive(name) {
        return Err(CanonicalError::Directive(format!(
            "unknown directive ':{name}' — not in the closed registry"
        )));
    }
    Ok(())
}

/// Split "name{attrs}" (or "name") into (name, attrs-source-without-braces).
fn split_name_attrs(s: &str) -> (String, &str) {
    let s = s.trim();
    if let Some(open) = s.find('{') {
        let name = s[..open].trim().to_string();
        let attrs = if let Some(close) = s.rfind('}') {
            &s[open + 1..close]
        } else {
            &s[open + 1..]
        };
        (name, attrs)
    } else {
        (s.to_string(), "")
    }
}

/// Parse `#id key=value key2="quoted value"` into (id, attrs).
fn parse_attrs(src: &str) -> Result<(Option<String>, BTreeMap<String, String>)> {
    let mut id = None;
    let mut attrs = BTreeMap::new();
    for token in tokenize(src) {
        if let Some(rest) = token.strip_prefix('#') {
            id = Some(rest.to_string());
        } else if let Some(eq) = token.find('=') {
            let key = token[..eq].trim().to_string();
            let mut val = token[eq + 1..].trim().to_string();
            if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
                val = val[1..val.len() - 1].to_string();
            }
            attrs.insert(key, val);
        }
        // bare tokens with neither # nor = are ignored
    }
    Ok((id, attrs))
}

/// Whitespace tokenizer that keeps double-quoted values intact.
fn tokenize(src: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in src.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_container_with_attrs_and_prose() {
        let body = ":::acceptance-criterion{#WORD-5-ac-1 status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8}\nOn first load, the app presents a modal.\n:::";
        let ds = parse(body).unwrap();
        assert_eq!(ds.len(), 1);
        let d = &ds[0];
        assert_eq!(d.name, "acceptance-criterion");
        assert_eq!(d.id.as_deref(), Some("WORD-5-ac-1"));
        assert_eq!(d.attr("status"), Some("met"));
        assert_eq!(d.attr("verifiedBy"), Some("did:atomic:lee"));
        assert_eq!(d.body, "On first load, the app presents a modal.");
    }

    #[test]
    fn parses_task_with_nested_file_ref() {
        let body = ":::task{#t1 status=done satisfies=WORD-5-ac-1}\nAdd modal.\n::file-ref{path=src/App.tsx}\n:::";
        let ds = parse(body).unwrap();
        let t = &ds[0];
        assert_eq!(t.name, "task");
        assert_eq!(t.children.len(), 1);
        assert_eq!(t.children[0].name, "file-ref");
        assert_eq!(t.children[0].attr("path"), Some("src/App.tsx"));
        assert_eq!(t.body, "Add modal.");
    }

    #[test]
    fn quoted_values_keep_spaces() {
        let body = ":::why{}\nprose\n:::";
        let ds = parse(body).unwrap();
        assert_eq!(ds[0].name, "why");
        assert_eq!(ds[0].body, "prose");
    }

    #[test]
    fn unknown_directive_is_error() {
        let body = ":::whatever{}\nx\n:::";
        assert!(parse(body).is_err());
    }

    #[test]
    fn unterminated_container_is_error() {
        let body = ":::why{}\nno close here";
        assert!(parse(body).is_err());
    }
}
