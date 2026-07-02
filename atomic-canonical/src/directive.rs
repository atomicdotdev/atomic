//! Container/leaf directive parser.
//!
//! The body's typed structure is explicit and local: the lift reads a node's
//! type off the directive name, never off heading position (the fragile
//! "positional lift" the doc rejects). All three directive forms are supported:
//!   - container: `:::name{#id key=value ...}` … prose … `:::`
//!   - leaf:      `::name{key=value ...}`  (edges only, no body)
//!   - inline:    `:name[label]{key=value ...}`  (names a node inside prose)
//!
//! Inline directives are recognized *inside container prose* and surface as
//! children of the container while the prose keeps them verbatim — the graph
//! stores the edge, the render resolves it (reference, don't embed). Because
//! prose is the unconstrained slot, an inline pattern whose name is not in
//! [`vocab::INLINE_DIRECTIVE_NAMES`] stays prose instead of erroring (a reason
//! mentioning `did:atomic:lee` or `foo:bar[0]` must never fail the lift).

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
    /// The `[label]` of an inline directive. `None` for container/leaf forms.
    pub label: Option<String>,
    /// Leaf and inline directives nested inside this container.
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
            let body = body_lines.join("\n").trim().to_string();
            // Inline directives inside the prose surface as children; the
            // prose keeps them verbatim (store the edge, render resolves it).
            children.extend(parse_inline(&body)?);
            out.push(Directive {
                name,
                id,
                attrs,
                body,
                label: None,
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
        label: None,
        children: Vec::new(),
    }))
}

/// Extract inline directives (`:name[label]{attrs}`) from running prose.
///
/// Recognition rules keep prose safe:
/// - the `:` must sit at a word boundary (start of text, or after a
///   non-alphanumeric character) — so `did:atomic:lee` never matches;
/// - the name must be immediately followed by `[label]` — a bare `:word`
///   is prose;
/// - the name must be in the inline registry — `:unknown[x]` stays prose,
///   because prose is the unconstrained slot.
pub fn parse_inline(text: &str) -> Result<Vec<Directive>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        // Word boundary: previous byte must not be alphanumeric or ':' (which
        // would make this the tail of a URN/DID or a block-directive marker).
        if i > 0 {
            let prev = bytes[i - 1] as char;
            if prev.is_ascii_alphanumeric() || prev == ':' {
                i += 1;
                continue;
            }
        }
        // Read the name: [a-z0-9-]+ immediately after ':'.
        let name_start = i + 1;
        let mut j = name_start;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
            j += 1;
        }
        if j == name_start || j >= bytes.len() || bytes[j] != b'[' {
            i += 1;
            continue;
        }
        let name = &text[name_start..j];
        if !vocab::is_known_inline_directive(name) {
            i += 1;
            continue;
        }
        // Label: up to the matching ']' (no nesting in labels).
        let label_start = j + 1;
        let Some(label_len) = text[label_start..].find(']') else {
            return Err(CanonicalError::Directive(format!(
                "unterminated inline directive ':{name}[' (missing closing ']')"
            )));
        };
        let label = text[label_start..label_start + label_len].to_string();
        let mut end = label_start + label_len + 1;
        // Optional immediate `{attrs}`.
        let mut attrs_src = "";
        if bytes.get(end) == Some(&b'{') {
            let Some(close) = text[end + 1..].find('}') else {
                return Err(CanonicalError::Directive(format!(
                    "unterminated inline directive ':{name}[…]{{' (missing closing '}}')"
                )));
            };
            attrs_src = &text[end + 1..end + 1 + close];
            end = end + 1 + close + 1;
        }
        let (id, attrs) = parse_attrs(attrs_src)?;
        out.push(Directive {
            name: name.to_string(),
            id,
            attrs,
            body: String::new(),
            label: Some(label),
            children: Vec::new(),
        });
        i = end;
    }
    Ok(out)
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

    // Inline directives

    #[test]
    fn inline_ref_in_container_prose_becomes_child() {
        let body = ":::why\nWe follow :ref[the storage decision]{to=urn:atomic:memory:01J edge=depends} here.\n:::";
        let ds = parse(body).unwrap();
        let why = &ds[0];
        // Prose keeps the inline verbatim (reference, don't embed).
        assert!(why.body.contains(":ref[the storage decision]"));
        assert_eq!(why.children.len(), 1);
        let r = &why.children[0];
        assert_eq!(r.name, "ref");
        assert_eq!(r.label.as_deref(), Some("the storage decision"));
        assert_eq!(r.attr("to"), Some("urn:atomic:memory:01J"));
        assert_eq!(r.attr("edge"), Some("depends"));
    }

    #[test]
    fn inline_parse_extracts_multiple() {
        let text = "see :ref[a]{to=urn:atomic:intent:1 edge=depends} and :ref[b]{to=urn:atomic:intent:2 edge=blockedBy}";
        let ds = parse_inline(text).unwrap();
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].label.as_deref(), Some("a"));
        assert_eq!(ds[1].attr("edge"), Some("blockedBy"));
    }

    #[test]
    fn dids_urns_and_unregistered_names_stay_prose() {
        // Colons inside identifiers must never match (word boundary), and an
        // unregistered inline name is prose, not an error.
        let text = "did:atomic:lee wrote urn:atomic:intent:x; :unknown[thing]{a=b} and foo:bar[0] stay prose";
        let ds = parse_inline(text).unwrap();
        assert!(ds.is_empty(), "got {ds:?}");
    }

    #[test]
    fn inline_without_label_bracket_is_prose() {
        let ds = parse_inline("a plain :ref mention with no bracket").unwrap();
        assert!(ds.is_empty());
    }

    #[test]
    fn unterminated_inline_label_is_error() {
        assert!(parse_inline("bad :ref[never closed").is_err());
    }

    #[test]
    fn unterminated_inline_attrs_is_error() {
        assert!(parse_inline("bad :ref[x]{to=urn:atomic:intent:1").is_err());
    }
}
