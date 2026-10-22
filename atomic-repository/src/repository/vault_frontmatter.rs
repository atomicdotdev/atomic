//! Vault frontmatter parsing.
//!
//! A vault file is markdown with an optional `---`-delimited YAML block in
//! front. [`parse_frontmatter_strict`] reads that block as YAML 1.2 and
//! refuses the parts of YAML a vault entry has no use for: anchors, aliases,
//! tags, merge keys, duplicate keys, non-string keys, multiple documents and
//! a non-mapping top level, plus inputs past a size and nesting cap.
//!
//! The vault used to read frontmatter with a line-based parser that only
//! understood `key: value` lines. Files under `kinds/` are new and are read
//! strictly. Every other file is read by both parsers for one release; when
//! they disagree the legacy result is kept, so no stored entry changes
//! meaning under the user's feet, and the disagreement is logged.

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};
use serde_json::{Map, Value};

/// Largest frontmatter block, in bytes, that [`parse_frontmatter_strict`]
/// accepts.
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;

/// Deepest nesting of mappings and sequences, counting the top-level
/// mapping, that [`parse_frontmatter_strict`] accepts.
const MAX_FRONTMATTER_DEPTH: usize = 32;

/// Keys the materializer writes into every file; they are rebuilt from the
/// entry itself, never stored as user frontmatter.
const SYSTEM_KEYS: [&str; 4] = ["entry_type", "content_hash", "created_at", "updated_at"];

/// Why [`parse_frontmatter_strict`] refused a frontmatter block.
///
/// Lines and columns are 1-based and relative to the frontmatter block
/// (the line after the opening `---` is line 1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrontmatterError {
    #[error("frontmatter is {size} bytes; the limit is {limit}")]
    TooLarge { size: usize, limit: usize },
    #[error("invalid YAML at line {line}, column {col}: {message}")]
    Syntax {
        line: usize,
        col: usize,
        message: String,
    },
    #[error("line {line}, column {col}: anchors are not allowed in frontmatter")]
    Anchor { line: usize, col: usize },
    #[error("line {line}, column {col}: aliases are not allowed in frontmatter")]
    Alias { line: usize, col: usize },
    #[error("line {line}, column {col}: tag `{tag}` is not allowed in frontmatter")]
    Tag {
        line: usize,
        col: usize,
        tag: String,
    },
    #[error("line {line}, column {col}: merge keys (`<<`) are not allowed in frontmatter")]
    MergeKey { line: usize, col: usize },
    #[error("line {line}, column {col}: duplicate key `{key}`")]
    DuplicateKey {
        key: String,
        line: usize,
        col: usize,
    },
    #[error("line {line}, column {col}: key `{key}` is not a string")]
    NonStringKey {
        key: String,
        line: usize,
        col: usize,
    },
    #[error("line {line}, column {col}: keys must be scalars, not mappings or sequences")]
    ComplexKey { line: usize, col: usize },
    #[error("line {line}, column {col}: nesting deeper than {limit} levels")]
    TooDeep {
        line: usize,
        col: usize,
        limit: usize,
    },
    #[error("line {line}, column {col}: frontmatter must be a single YAML document")]
    MultipleDocuments { line: usize, col: usize },
    #[error("line {line}, column {col}: frontmatter must be a mapping of keys to values")]
    NotAMapping { line: usize, col: usize },
}

/// Parse a frontmatter block (the text between the `---` delimiters) as
/// YAML 1.2.
///
/// Plain scalars resolve through the YAML 1.2 core schema: `null`, `~` and
/// the empty value are null, `true`/`false` (also `True`, `TRUE`, ...) are
/// booleans, decimal, `0o` and `0x` integers and decimal floats are numbers.
/// Everything else, including `yes`/`no`/`on`/`off`, is a string, and quoted
/// or block scalars are always strings. `.inf` and `.nan` have no JSON
/// number, so they are kept as the strings they were written as.
///
/// The four system keys the materializer writes (`entry_type`,
/// `content_hash`, `created_at`, `updated_at`) are removed from the top
/// level of the result.
///
/// # Examples
///
/// ```
/// use atomic_repository::parse_frontmatter_strict;
///
/// let fm = parse_frontmatter_strict("title: Use redb\nsupersedes:\n  - A\n  - B\n").unwrap();
/// assert_eq!(fm["title"], "Use redb");
/// assert_eq!(fm["supersedes"], serde_json::json!(["A", "B"]));
/// assert!(parse_frontmatter_strict("a: 1\na: 2\n").is_err());
/// ```
pub fn parse_frontmatter_strict(yaml: &str) -> Result<Map<String, Value>, FrontmatterError> {
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(FrontmatterError::TooLarge {
            size: yaml.len(),
            limit: MAX_FRONTMATTER_BYTES,
        });
    }

    let mut builder = Builder::default();
    for event in Parser::new_from_str(yaml) {
        let (event, span) = event.map_err(|e| FrontmatterError::Syntax {
            line: e.marker().line(),
            col: e.marker().col() + 1,
            message: e.info().to_string(),
        })?;
        if builder.event(event, span)? {
            break;
        }
    }

    let mut map = builder.root.unwrap_or_default();
    for key in SYSTEM_KEYS {
        map.remove(key);
    }
    Ok(map)
}

/// A collection still being filled while events stream in.
enum Frame {
    Mapping {
        map: Map<String, Value>,
        /// The key whose value comes next, or `None` when a key comes next.
        key: Option<String>,
    },
    Sequence(Vec<Value>),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    root: Option<Map<String, Value>>,
    documents: usize,
}

fn position(span: &Span) -> (usize, usize) {
    (span.start.line(), span.start.col() + 1)
}

impl Builder {
    /// Feed one event. Returns `true` at the end of the stream.
    fn event(&mut self, event: Event<'_>, span: Span) -> Result<bool, FrontmatterError> {
        let (line, col) = position(&span);
        match event {
            Event::StreamEnd => return Ok(true),
            Event::StreamStart | Event::DocumentEnd | Event::Nothing => {}
            Event::DocumentStart(_) => {
                self.documents += 1;
                if self.documents > 1 {
                    return Err(FrontmatterError::MultipleDocuments { line, col });
                }
            }
            Event::Alias(_) => return Err(FrontmatterError::Alias { line, col }),
            Event::Scalar(value, style, anchor, tag) => {
                check_node(anchor, tag.as_deref(), line, col)?;
                if self.expects_key() {
                    let key = scalar_key(&value, style, line, col)?;
                    let Some(Frame::Mapping { map, key: pending }) = self.stack.last_mut() else {
                        unreachable!("expects_key checked the top frame");
                    };
                    if map.contains_key(&key) {
                        return Err(FrontmatterError::DuplicateKey { key, line, col });
                    }
                    *pending = Some(key);
                } else {
                    let value = match style {
                        ScalarStyle::Plain => resolve_plain(&value),
                        _ => Value::String(value.into_owned()),
                    };
                    self.complete(value, line, col)?;
                }
            }
            Event::SequenceStart(anchor, tag) => {
                self.open(anchor, tag.as_deref(), line, col)?;
                self.stack.push(Frame::Sequence(Vec::new()));
            }
            Event::MappingStart(anchor, tag) => {
                self.open(anchor, tag.as_deref(), line, col)?;
                self.stack.push(Frame::Mapping {
                    map: Map::new(),
                    key: None,
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let value = match self.stack.pop() {
                    Some(Frame::Sequence(items)) => Value::Array(items),
                    Some(Frame::Mapping { map, .. }) => Value::Object(map),
                    None => unreachable!("the parser balances collection events"),
                };
                self.complete(value, line, col)?;
            }
        }
        Ok(false)
    }

    fn expects_key(&self) -> bool {
        matches!(self.stack.last(), Some(Frame::Mapping { key: None, .. }))
    }

    fn open(
        &self,
        anchor: usize,
        tag: Option<&Tag>,
        line: usize,
        col: usize,
    ) -> Result<(), FrontmatterError> {
        check_node(anchor, tag, line, col)?;
        if self.expects_key() {
            return Err(FrontmatterError::ComplexKey { line, col });
        }
        if self.stack.len() >= MAX_FRONTMATTER_DEPTH {
            return Err(FrontmatterError::TooDeep {
                line,
                col,
                limit: MAX_FRONTMATTER_DEPTH,
            });
        }
        Ok(())
    }

    /// Place a finished value in its parent, or make it the document root.
    fn complete(&mut self, value: Value, line: usize, col: usize) -> Result<(), FrontmatterError> {
        match self.stack.last_mut() {
            Some(Frame::Sequence(items)) => items.push(value),
            Some(Frame::Mapping { map, key }) => {
                let key = key.take().expect("a value follows its key");
                map.insert(key, value);
            }
            None => match value {
                Value::Object(map) => self.root = Some(map),
                // A block holding only comments or blank lines.
                Value::Null if self.root.is_none() => {}
                _ => return Err(FrontmatterError::NotAMapping { line, col }),
            },
        }
        Ok(())
    }
}

fn check_node(
    anchor: usize,
    tag: Option<&Tag>,
    line: usize,
    col: usize,
) -> Result<(), FrontmatterError> {
    if anchor != 0 {
        return Err(FrontmatterError::Anchor { line, col });
    }
    if let Some(tag) = tag {
        return Err(FrontmatterError::Tag {
            line,
            col,
            tag: format!("{}{}", tag.handle, tag.suffix),
        });
    }
    Ok(())
}

fn scalar_key(
    value: &str,
    style: ScalarStyle,
    line: usize,
    col: usize,
) -> Result<String, FrontmatterError> {
    if style != ScalarStyle::Plain {
        return Ok(value.to_string());
    }
    if value == "<<" {
        return Err(FrontmatterError::MergeKey { line, col });
    }
    match resolve_plain(value) {
        Value::String(key) => Ok(key),
        _ => Err(FrontmatterError::NonStringKey {
            key: value.to_string(),
            line,
            col,
        }),
    }
}

/// Resolve a plain scalar through the YAML 1.2 core schema.
fn resolve_plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = core_int(s) {
        return Value::Number(n);
    }
    if is_core_float(s) {
        // Every core-schema float that is not `.inf`/`.nan` parses as a
        // finite f64 (or overflows to infinity, which JSON cannot hold).
        if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Value::Number(n);
        }
    }
    Value::String(s.to_string())
}

/// `[-+]?[0-9]+ | 0o[0-7]+ | 0x[0-9a-fA-F]+`, when it fits a JSON integer.
fn core_int(s: &str) -> Option<serde_json::Number> {
    let radix_digits = |prefix: &str, radix: u32| {
        let digits = s.strip_prefix(prefix)?;
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        u64::from_str_radix(digits, radix).ok().map(Into::into)
    };
    if let Some(n) = radix_digits("0o", 8).or_else(|| radix_digits("0x", 16)) {
        return Some(n);
    }
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>()
        .map(Into::into)
        .or_else(|_| {
            s.strip_prefix('+')
                .unwrap_or(s)
                .parse::<u64>()
                .map(Into::into)
        })
        .ok()
}

/// `[-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?`
///
/// The `.inf`/`.nan` forms are deliberately not matched: they stay strings.
fn is_core_float(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let all_digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => !frac.is_empty() && all_digits(frac),
        Some((int, frac)) => !int.is_empty() && all_digits(int) && all_digits(frac),
        None => !mantissa.is_empty() && all_digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && all_digits(e)
    });
    mantissa_ok && exponent_ok
}

// ── Writer ──────────────────────────────────────────────────────────────

/// Render a complete `kinds/` file: frontmatter between `---` lines, then
/// `body` verbatim.
///
/// Keys are written sorted; the system keys the materializer adds
/// (`entry_type`, `content_hash`, `created_at`, `updated_at`) are left out.
/// Strings are written bare when YAML reads them back unchanged and
/// JSON-quoted otherwise; arrays and objects are written as flow-style
/// JSON. The result always reads back through [`parse_frontmatter_strict`]
/// as `fm`.
///
/// # Examples
///
/// ```
/// use atomic_repository::{parse_frontmatter_strict, render_frontmatter_document};
///
/// let fm = serde_json::json!({"title": "Use redb", "status": "~", "refs": ["A"]});
/// let doc = render_frontmatter_document(fm.as_object().unwrap(), "Body.\n");
/// assert_eq!(doc, "---\nrefs: [\"A\"]\nstatus: \"~\"\ntitle: Use redb\n---\nBody.\n");
/// ```
pub fn render_frontmatter_document(fm: &Map<String, Value>, body: &str) -> String {
    let mut output = String::from("---\n");
    let mut keys: Vec<&String> = fm
        .keys()
        .filter(|k| !SYSTEM_KEYS.contains(&k.as_str()))
        .collect();
    keys.sort();
    if keys.is_empty() {
        // `---\n---\n` is not recognized as frontmatter; `{}` is an empty
        // mapping to both parsers.
        output.push_str("{}\n");
    }
    for key in keys {
        let key_text = if reads_back_as_string_key(key) {
            key.clone()
        } else {
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string())
        };
        write_frontmatter_field(&mut output, &key_text, &fm[key.as_str()]);
    }
    output.push_str("---\n");
    output.push_str(body);
    output
}

/// Write a single frontmatter field in YAML format.
///
/// `key` is written as given. A string value is JSON-quoted (JSON string
/// syntax is valid YAML and preserves escapes) whenever either parser would
/// read the bare text as anything other than that same string.
pub(super) fn write_frontmatter_field(output: &mut String, key: &str, value: &Value) {
    use std::fmt::Write;
    match value {
        Value::String(s) => {
            if needs_quotes(s) {
                let rendered = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
                let _ = writeln!(output, "{}: {}", key, rendered);
            } else {
                let _ = writeln!(output, "{}: {}", key, s);
            }
        }
        Value::Number(n) => {
            let _ = writeln!(output, "{}: {}", key, n);
        }
        Value::Bool(b) => {
            let _ = writeln!(output, "{}: {}", key, b);
        }
        Value::Array(arr) => {
            let rendered = serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string());
            let _ = writeln!(output, "{}: {}", key, rendered);
        }
        Value::Object(_) => {
            // Inline JSON for nested objects
            let _ = writeln!(output, "{}: {}", key, value);
        }
        Value::Null => {
            let _ = writeln!(output, "{}: null", key);
        }
    }
}

/// Whether `s`, written bare as a value, would read back as anything but
/// `s` under either parser.
fn needs_quotes(s: &str) -> bool {
    let has_edge_whitespace = s.chars().next().is_some_and(char::is_whitespace)
        || s.chars().last().is_some_and(char::is_whitespace);
    if s.contains(": ")
        || s.contains('#')
        || s.contains('\n')
        || s.contains('\r')
        || s.starts_with('{')
        || s.starts_with('[')
        || s.starts_with('"')
        || s.starts_with('\'')
        || has_edge_whitespace
        || parse_yaml_value(s) != Value::String(s.to_string())
    {
        return true;
    }
    // The YAML reading is the oracle: it covers the core-schema scalars
    // (`~`, `Null`, `True`, `0x1F`, the empty string, ...) and indicators
    // (`- x`, `*x`, `!x`, `|`, `x:`, ...) that the checks above miss.
    let mut probe = String::with_capacity(s.len() + 4);
    probe.push_str("v: ");
    probe.push_str(s);
    probe.push('\n');
    !matches!(
        parse_frontmatter_strict(&probe),
        Ok(map) if map.len() == 1 && map.get("v").and_then(Value::as_str) == Some(s)
    )
}

/// Whether `key`, written bare, reads back under the strict parser as the
/// string key `key`.
fn reads_back_as_string_key(key: &str) -> bool {
    let probe = format!("{key}: 0\n");
    !key.contains(['\n', '\r'])
        && matches!(
            parse_frontmatter_strict(&probe),
            Ok(map) if map.len() == 1 && map.contains_key(key)
        )
}

// ── Path-aware entry point ──────────────────────────────────────────────

/// Split a vault markdown file into its frontmatter (as a JSON object
/// string) and body.
///
/// `path` is the vault-relative path (`kinds/adr/01J….md`), when known.
/// Files under `kinds/` are parsed strictly; a block that does not parse is
/// logged and stored as `{}`. Every other file is parsed both strictly and
/// by the legacy line parser, and the legacy result wins (with a warning)
/// whenever the two disagree.
///
/// A file that does not start with `---` followed by a newline, or has no
/// closing `---` line, has no frontmatter: it returns `("{}", content)`.
pub(crate) fn parse_vault_frontmatter_at(path: Option<&str>, content: &str) -> (String, String) {
    let Some((yaml, body)) = split_frontmatter(content) else {
        return ("{}".to_string(), content.to_string());
    };
    let map = resolve_frontmatter(path, yaml);
    let json = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
    (json, body.to_string())
}

/// `(frontmatter, body)` for a file that opens with a `---` line and has a
/// closing one.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let after_first = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let rest = &content[after_first..];

    let (close_idx, skip_len) = if let Some(idx) = rest.find("\n---\n") {
        (idx, 5) // "\n---\n" is 5 bytes
    } else if let Some(idx) = rest.find("\n---\r\n") {
        (idx, 6) // "\n---\r\n" is 6 bytes
    } else if rest.ends_with("\n---") {
        (rest.len() - 3, 3)
    } else {
        return None;
    };

    let yaml = &rest[..close_idx];
    let body = content
        .get(after_first + close_idx + skip_len..)
        .unwrap_or("");
    Some((yaml, body))
}

fn resolve_frontmatter(path: Option<&str>, yaml: &str) -> Map<String, Value> {
    let file = path.unwrap_or("<vault file>");
    if path.is_some_and(|p| p.starts_with("kinds/")) {
        return parse_frontmatter_strict(yaml).unwrap_or_else(|error| {
            log::warn!("vault: ignoring frontmatter of {file}: {error}");
            Map::new()
        });
    }

    let legacy = legacy_frontmatter_to_map(yaml);
    match parse_frontmatter_strict(yaml) {
        Ok(strict) if strict == legacy => strict,
        Ok(strict) => {
            let key = first_differing_key(&strict, &legacy).unwrap_or_default();
            log::warn!(
                "vault: YAML and legacy frontmatter parsers disagree on key `{key}` in {file}; \
                 keeping the legacy reading"
            );
            legacy
        }
        Err(error) => {
            log::warn!(
                "vault: frontmatter of {file} is not valid YAML ({error}); \
                 keeping the legacy reading"
            );
            legacy
        }
    }
}

fn first_differing_key<'a>(
    a: &'a Map<String, Value>,
    b: &'a Map<String, Value>,
) -> Option<&'a str> {
    let mut keys: Vec<&str> = a.keys().chain(b.keys()).map(String::as_str).collect();
    keys.sort_unstable();
    keys.into_iter().find(|k| a.get(*k) != b.get(*k))
}

// ── Legacy line parser ──────────────────────────────────────────────────

/// Convert frontmatter to JSON one `key: value` line at a time.
///
/// Handles:
/// - `key: value` -> `{"key": "value"}`
/// - `key: 123` -> `{"key": 123}`
/// - `key: true/false` -> `{"key": true/false}`
/// - `key: [a, b, c]` -> `{"key": ["a", "b", "c"]}`
/// - `key: {json}` -> `{"key": {json}}`
///
/// It is not YAML: indentation is ignored (nested keys are flattened into
/// the top level), block lists are dropped, `key:` reads as `""`, and a
/// repeated key keeps its last value. It reads every form the materializer
/// writes, which is why it stays as the fallback while
/// [`parse_frontmatter_strict`] takes over.
// TODO: remove one release after the YAML parser ships
fn legacy_frontmatter_to_map(yaml: &str) -> Map<String, Value> {
    let mut map = Map::new();

    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on first ': '
        let Some(colon_idx) = line.find(": ") else {
            // Handle `key:` with no value (treated as empty string)
            if let Some(key) = line.strip_suffix(':') {
                map.insert(key.trim().to_string(), Value::String(String::new()));
            }
            continue;
        };

        let key = line[..colon_idx].trim().to_string();
        let value_str = line[colon_idx + 2..].trim();

        // Skip system fields that we inject during materialization
        // (they'll be reconstructed, not stored in frontmatter_json)
        if SYSTEM_KEYS.contains(&key.as_str()) {
            continue;
        }

        let value = parse_yaml_value(value_str);
        map.insert(key, value);
    }

    map
}

/// Parse one legacy frontmatter value string into a JSON value.
///
/// The writer also uses it to decide which strings need quoting.
// TODO: remove one release after the YAML parser ships
fn parse_yaml_value(s: &str) -> Value {
    // Quoted values remain strings even when their contents look like another
    // scalar type. Decode JSON-style double-quoted escapes losslessly.
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return serde_json::from_str::<String>(s)
            .map(Value::String)
            .unwrap_or_else(|_| Value::String(s[1..s.len() - 1].to_string()));
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return Value::String(s[1..s.len() - 1].replace("''", "'"));
    }

    // Try as integer
    if let Ok(n) = s.parse::<i64>() {
        return Value::Number(n.into());
    }
    // Try as float
    if let Ok(n) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(n) {
            return Value::Number(n);
        }
    }

    // Try as boolean / null
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }

    // Materialized arrays/objects use JSON syntax, which is also valid YAML.
    // Parse it first so quoted commas and nested values remain lossless.
    if (s.starts_with('[') && s.ends_with(']')) || (s.starts_with('{') && s.ends_with('}')) {
        if let Ok(value) = serde_json::from_str::<Value>(s) {
            return value;
        }
    }

    // Also accept simple hand-authored YAML arrays such as [a, b, c].
    if s.starts_with('[') && s.ends_with(']') {
        let inner = s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Value::Array(Vec::new());
        }
        let items: Vec<Value> = inner
            .split(',')
            .map(|item| parse_yaml_value(item.trim()))
            .collect();
        return Value::Array(items);
    }

    // Default: string
    Value::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::vault::{VaultEntry, VaultEntryType};
    use serde_json::json;

    fn strict(yaml: &str) -> Value {
        Value::Object(parse_frontmatter_strict(yaml).unwrap())
    }

    fn legacy(yaml: &str) -> Value {
        Value::Object(legacy_frontmatter_to_map(yaml))
    }

    /// What a non-`kinds/` file stores.
    fn stored(yaml: &str) -> Value {
        Value::Object(resolve_frontmatter(Some("memory/x.md"), yaml))
    }

    /// The two parsers disagree on `yaml`, and the dual run keeps legacy.
    fn assert_falls_back(yaml: &str) {
        let legacy = legacy(yaml);
        assert_ne!(
            parse_frontmatter_strict(yaml).map(Value::Object).ok(),
            Some(legacy.clone()),
            "{yaml:?}"
        );
        assert_eq!(stored(yaml), legacy, "{yaml:?}");
    }

    #[test]
    fn compatibility_table() {
        // `key:` with no value
        assert_eq!(strict("key:\n"), json!({"key": null}));
        assert_eq!(legacy("key:\n"), json!({"key": ""}));
        assert_falls_back("key:\n");

        // An unquoted `: ` inside a value
        assert!(matches!(
            parse_frontmatter_strict("title: a: b\n"),
            Err(FrontmatterError::Syntax { line: 1, .. })
        ));
        assert_eq!(legacy("title: a: b\n"), json!({"title": "a: b"}));
        assert_falls_back("title: a: b\n");

        // A trailing comment
        assert_eq!(strict("note: x # y\n"), json!({"note": "x"}));
        assert_eq!(legacy("note: x # y\n"), json!({"note": "x # y"}));
        assert_falls_back("note: x # y\n");

        // A repeated key
        assert!(matches!(
            parse_frontmatter_strict("a: 1\nb: 2\na: 3\n"),
            Err(FrontmatterError::DuplicateKey { ref key, line: 3, col: 1 }) if key == "a"
        ));
        assert_eq!(legacy("a: 1\nb: 2\na: 3\n"), json!({"a": 3, "b": 2}));
        assert_falls_back("a: 1\nb: 2\na: 3\n");

        // Conflict markers left by a merge
        let conflicted = "title: x\n<<<<<<< ours\nstatus: a\n=======\nstatus: b\n>>>>>>> theirs\n";
        assert!(parse_frontmatter_strict(conflicted).is_err());
        assert_falls_back(conflicted);

        // An indented key nests
        let nested = "parent:\n  child: x\n";
        assert_eq!(strict(nested), json!({"parent": {"child": "x"}}));
        assert_eq!(legacy(nested), json!({"parent": "", "child": "x"}));
        assert_falls_back(nested);
    }

    #[test]
    fn kinds_files_are_strict_only() {
        let at = |yaml: &str| Value::Object(resolve_frontmatter(Some("kinds/adr/01J.md"), yaml));
        assert_eq!(at("title: a: b\n"), json!({}));
        assert_eq!(at("a: 1\na: 2\n"), json!({}));
        assert_eq!(at("key:\n"), json!({"key": null}));
        assert_eq!(at("refs:\n  - A\n"), json!({"refs": ["A"]}));
        // Elsewhere, the same block list falls back to legacy (which drops it).
        assert_eq!(stored("refs:\n  - A\n"), json!({"refs": ""}));
    }

    #[test]
    fn agreeing_parsers_store_the_strict_result() {
        let yaml = "name: x\ncount: 3\nok: true\nlabels: [a, b]\n";
        assert_eq!(strict(yaml), legacy(yaml));
        assert_eq!(
            stored(yaml),
            json!({"name": "x", "count": 3, "ok": true, "labels": ["a", "b"]})
        );
    }

    #[test]
    fn block_and_flow_lists_are_equivalent() {
        assert_eq!(
            strict("supersedes:\n  - A\n  - B\n"),
            strict("supersedes: [A, B]\n")
        );
        assert_eq!(
            strict("supersedes: [A, B]\n"),
            json!({"supersedes": ["A", "B"]})
        );
        assert_eq!(
            strict("xs:\n- 1\n- [2, {k: v}]\n"),
            json!({"xs": [1, [2, {"k": "v"}]]})
        );
    }

    #[test]
    fn core_schema_scalars() {
        let cases = [
            ("~", json!(null)),
            ("null", json!(null)),
            ("Null", json!(null)),
            ("NULL", json!(null)),
            ("", json!(null)),
            ("true", json!(true)),
            ("True", json!(true)),
            ("TRUE", json!(true)),
            ("false", json!(false)),
            ("FALSE", json!(false)),
            ("tRUE", json!("tRUE")),
            ("0", json!(0)),
            ("-17", json!(-17)),
            ("+17", json!(17)),
            ("007", json!(7)),
            ("0o17", json!(15)),
            ("0x1F", json!(31)),
            ("0x", json!("0x")),
            ("0o8", json!("0o8")),
            ("18446744073709551615", json!(u64::MAX)),
            ("1.5", json!(1.5)),
            ("-.5", json!(-0.5)),
            ("1.", json!(1.0)),
            ("1e3", json!(1000.0)),
            ("6.02E+23", json!(6.02e23)),
            ("1e400", json!("1e400")),
            (".inf", json!(".inf")),
            ("-.Inf", json!("-.Inf")),
            (".nan", json!(".nan")),
            ("1_000", json!("1_000")),
            ("yes", json!("yes")),
            ("no", json!("no")),
            ("on", json!("on")),
            ("off", json!("off")),
            ("y", json!("y")),
            ("'true'", json!("true")),
            ("\"1\"", json!("1")),
            ("'it''s'", json!("it's")),
            ("\"a\\tb\\u00e9\"", json!("a\tbé")),
        ];
        for (raw, expected) in cases {
            assert_eq!(strict(&format!("v: {raw}\n"))["v"], expected, "{raw:?}");
        }
    }

    #[test]
    fn norway_stays_a_string() {
        assert_eq!(strict("country: no\n"), json!({"country": "no"}));
    }

    #[test]
    fn block_scalars_are_strings() {
        assert_eq!(
            strict("a: |\n  one\n  two\nb: >\n  one\n  two\nc: |-\n  42\n"),
            json!({"a": "one\ntwo\n", "b": "one two\n", "c": "42"})
        );
    }

    #[test]
    fn rejections() {
        use FrontmatterError as E;
        let err = |yaml: &str| parse_frontmatter_strict(yaml).unwrap_err();
        assert!(matches!(err("a: &x 1\n"), E::Anchor { line: 1, .. }));
        assert!(matches!(err("a: &x [1]\n"), E::Anchor { .. }));
        assert!(matches!(err("a: &x 1\nb: *x\n"), E::Anchor { .. }));
        assert!(matches!(err("b: *x\n"), E::Syntax { .. } | E::Alias { .. }));
        assert!(matches!(err("a: !!str 1\n"), E::Tag { ref tag, .. } if tag.ends_with("str")));
        assert!(matches!(err("a: !custom x\n"), E::Tag { .. }));
        assert!(matches!(err("a: !!map {}\n"), E::Tag { .. }));
        assert!(matches!(err("<<: {a: 1}\n"), E::MergeKey { .. }));
        assert!(matches!(
            err("x:\n  <<: {a: 1}\n"),
            E::MergeKey { line: 2, .. }
        ));
        assert!(matches!(
            err("x:\n  a: 1\n  a: 2\n"),
            E::DuplicateKey { line: 3, .. }
        ));
        assert!(matches!(err("x: [{a: 1, a: 2}]\n"), E::DuplicateKey { .. }));
        assert!(matches!(err("1: x\n"), E::NonStringKey { .. }));
        assert!(matches!(err("true: x\n"), E::NonStringKey { .. }));
        assert!(matches!(err("~: x\n"), E::NonStringKey { .. }));
        assert!(matches!(err("? [a]\n: x\n"), E::ComplexKey { .. }));
        assert!(matches!(err("? {a: 1}\n: x\n"), E::ComplexKey { .. }));
        assert!(matches!(
            err("a: 1\n---\nb: 2\n"),
            E::MultipleDocuments { .. }
        ));
        assert!(matches!(err("- a\n- b\n"), E::NotAMapping { .. }));
        assert!(matches!(err("just a string\n"), E::NotAMapping { .. }));

        // Quoted lookalikes are ordinary string keys.
        assert_eq!(strict("'<<': x\n'1': y\n"), json!({"<<": "x", "1": "y"}));
    }

    #[test]
    fn depth_limit() {
        let nested = |depth: usize| {
            // The top-level mapping is one level; each `[` adds one.
            let inner = depth - 1;
            format!("a: {}{}\n", "[".repeat(inner), "]".repeat(inner))
        };
        assert!(parse_frontmatter_strict(&nested(MAX_FRONTMATTER_DEPTH)).is_ok());
        assert!(matches!(
            parse_frontmatter_strict(&nested(MAX_FRONTMATTER_DEPTH + 1)),
            Err(FrontmatterError::TooDeep { limit: 32, .. })
        ));
        // Block nesting counts the same way.
        let mut block = String::new();
        for level in 0..=MAX_FRONTMATTER_DEPTH {
            block.push_str(&format!("{}k{level}:\n", "  ".repeat(level)));
        }
        assert!(matches!(
            parse_frontmatter_strict(&block),
            Err(FrontmatterError::TooDeep { .. })
        ));
    }

    #[test]
    fn size_limit() {
        let fits = format!("a: {}\n", "x".repeat(MAX_FRONTMATTER_BYTES - 4));
        assert_eq!(fits.len(), MAX_FRONTMATTER_BYTES);
        assert!(parse_frontmatter_strict(&fits).is_ok());
        let over = format!("a: {}\n", "x".repeat(MAX_FRONTMATTER_BYTES));
        assert!(matches!(
            parse_frontmatter_strict(&over),
            Err(FrontmatterError::TooLarge { limit: 65536, .. })
        ));
    }

    #[test]
    fn empty_and_comment_only_blocks_are_empty_maps() {
        assert_eq!(strict(""), json!({}));
        assert_eq!(strict("\n\n"), json!({}));
        assert_eq!(strict("# just a comment\n"), json!({}));
    }

    #[test]
    fn system_keys_are_stripped_at_the_top_level_only() {
        let yaml = "name: x\nentry_type: memory\ncontent_hash: ABC\ncreated_at: t\n\
                    updated_at: t\nnested: {entry_type: keep}\n";
        assert_eq!(
            strict(yaml),
            json!({"name": "x", "nested": {"entry_type": "keep"}})
        );
    }

    #[test]
    fn delimiters_are_unchanged() {
        let parse = |content: &str| {
            let (json, body) = parse_vault_frontmatter_at(None, content);
            (serde_json::from_str::<Value>(&json).unwrap(), body)
        };
        assert_eq!(
            parse("---\na: 1\n---\nbody\n"),
            (json!({"a": 1}), "body\n".into())
        );
        assert_eq!(
            parse("---\r\na: 1\r\n---\r\nbody\r\n"),
            (json!({"a": 1}), "body\r\n".into())
        );
        assert_eq!(parse("---\na: 1\n---"), (json!({"a": 1}), String::new()));
        assert_eq!(
            parse("no frontmatter\n"),
            (json!({}), "no frontmatter\n".into())
        );
        assert_eq!(
            parse("---\na: 1\nunclosed\n"),
            (json!({}), "---\na: 1\nunclosed\n".into())
        );
        assert_eq!(
            parse("---\na: 1\n---\nx\n---\ny\n"),
            (json!({"a": 1}), "x\n---\ny\n".into())
        );
    }

    fn rendered_frontmatter(frontmatter: &Value) -> String {
        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"body\n".to_vec(),
            frontmatter.to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );
        let markdown = crate::repository::vault::render_entry_to_markdown(&entry);
        let (yaml, body) = split_frontmatter(&markdown).expect("rendered with frontmatter");
        assert_eq!(body, "body\n");
        yaml.to_string()
    }

    /// Everything the materializer writes reads back identically through
    /// both parsers.
    #[test]
    fn writer_output_round_trips_through_both_parsers() {
        let values = [
            json!("plain"),
            json!("two words"),
            json!("a: b"),
            json!("key: value: more"),
            json!("x # not a comment"),
            json!("#hashtag"),
            json!("issue#12"),
            json!(" leading"),
            json!("trailing "),
            json!("\ttab"),
            json!("line\nbreak"),
            json!("crlf\r\nbreak"),
            json!("quote \" inside"),
            json!("\"starts quoted"),
            json!("'single"),
            json!("back\\slash"),
            json!("[draft]"),
            json!("{brace"),
            json!("true"),
            json!("false"),
            json!("null"),
            json!("123"),
            json!("-1.5"),
            json!("1e3"),
            json!("yes"),
            json!("no"),
            json!("a:b"),
            json!("http://example.com/x?y=1"),
            json!("2025-07-15T12:00:00Z"),
            json!("continuouslee <lee@atomic.dev>"),
            json!("中文 ✓ émoji 🎉"),
            json!("bell\u{7}"),
            // Bare, these read as something else under YAML 1.2.
            json!(""),
            json!("~"),
            json!("Null"),
            json!("NULL"),
            json!("True"),
            json!("FALSE"),
            json!("0x1F"),
            json!("0o17"),
            json!("+1"),
            json!(".5"),
            json!("trailing:"),
            json!("- item"),
            json!("-"),
            json!("*alias"),
            json!("&anchor"),
            json!("!tag"),
            json!("|"),
            json!(">"),
            json!("%directive"),
            json!("@at"),
            json!("`tick"),
            json!("? key"),
            json!(".inf"),
            json!("cr\ronly"),
            json!(0),
            json!(-42),
            json!(i64::MAX),
            json!(i64::MIN),
            json!(1.5),
            json!(-0.25),
            json!(1e20),
            json!(1.5e-7),
            json!(100.0),
            json!(true),
            json!(false),
            json!(null),
            json!([]),
            json!({}),
            json!(["a", "b"]),
            json!(["true", "a,b", 123, false, null, 1.5]),
            json!(["a: b", "#x", " pad ", "[x]", "\"q\""]),
            json!([[1, [2, [3]]], {"k": ["v"]}]),
            json!({"status": "null", "n": {"deep": [1, {"x": "y: z"}]}}),
            json!({"key with spaces": 1, "a:b": "c", "#": "hash"}),
        ];
        for (i, value) in values.iter().enumerate() {
            let frontmatter = json!({ "v": value, "name": format!("entry-{i}") });
            let yaml = rendered_frontmatter(&frontmatter);
            assert_eq!(
                parse_frontmatter_strict(&yaml).map(Value::Object),
                Ok(frontmatter.clone()),
                "strict reading of {yaml:?}"
            );
            assert_eq!(legacy(&yaml), frontmatter, "legacy reading of {yaml:?}");
        }

        let all: Map<String, Value> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("k{i:02}"), v.clone()))
            .collect();
        let all = Value::Object(all);
        let yaml = rendered_frontmatter(&all);
        assert_eq!(strict(&yaml), all);
        assert_eq!(legacy(&yaml), all);
    }

    /// Hand-written values that both parsers accept but read differently.
    /// The writer never produces them (it quotes these strings), but files
    /// written by hand still do; outside `kinds/` they keep the legacy
    /// reading, with a warning.
    #[test]
    fn hand_written_forms_yaml_reads_differently() {
        let cases = [
            ("v: \n", json!(""), json!(null)),
            ("v: ~\n", json!("~"), json!(null)),
            ("v: Null\n", json!("Null"), json!(null)),
            ("v: True\n", json!("True"), json!(true)),
            ("v: FALSE\n", json!("FALSE"), json!(false)),
            ("v: 0x1F\n", json!("0x1F"), json!(31)),
            ("v: 0o17\n", json!("0o17"), json!(15)),
        ];
        for (yaml, legacy_value, yaml_value) in cases {
            assert_eq!(legacy(yaml), json!({ "v": legacy_value }), "{yaml:?}");
            assert_eq!(strict(yaml), json!({ "v": yaml_value }), "{yaml:?}");
            assert_eq!(stored(yaml), legacy(yaml), "{yaml:?}");
        }
        for yaml in ["v: trailing:\n", "v: - item\n"] {
            assert!(parse_frontmatter_strict(yaml).is_err(), "{yaml:?}");
            assert_falls_back(yaml);
        }
    }

    /// Numbers the writer emits that the two parsers read differently.
    #[test]
    fn writer_numbers_past_i64() {
        // An integer past `i64::MAX`: YAML keeps it exact, legacy rounds it
        // through f64. The dual run keeps that (pre-existing) rounding.
        let yaml = rendered_frontmatter(&json!({ "v": u64::MAX }));
        assert_eq!(strict(&yaml), json!({ "v": u64::MAX }));
        assert_eq!(legacy(&yaml), json!({ "v": u64::MAX as f64 }));
        assert_eq!(stored(&yaml), legacy(&yaml));
    }

    #[test]
    fn rendered_documents_round_trip_strictly() {
        let fm = json!({
            "uid": "01J9ZQ3V7M4K2X8R6T5N0B1C2D",
            "title": "Use redb: not sqlite",
            "status": "~",
            "empty": "",
            "flag": "True",
            "count": 3,
            "ratio": 0.5,
            "ok": false,
            "none": null,
            "supersedes": ["01J9ZQ3V7M4K2X8R6T5N0B1C2E", "- x", ""],
            "meta": {"a": [1, {"b": "c: d"}], "e": null},
            "key: with colon": "x",
            "1": "numeric key",
            "true": "bool key",
            "- dash": "indicator key",
            "#hash": "y",
        });
        let fm = fm.as_object().unwrap();
        let doc = render_frontmatter_document(fm, "# Body\n\nText.\n");
        let (yaml, body) = split_frontmatter(&doc).unwrap();
        assert_eq!(body, "# Body\n\nText.\n");
        assert_eq!(parse_frontmatter_strict(yaml).as_ref(), Ok(fm), "{yaml}");
        assert_eq!(
            parse_vault_frontmatter_at(Some("kinds/adr/01J9ZQ3V7M4K2X8R6T5N0B1C2D.md"), &doc),
            (
                serde_json::to_string(fm).unwrap(),
                "# Body\n\nText.\n".to_string()
            )
        );

        // Sorted keys, bare where safe.
        let simple = json!({"b": "two", "a": 1});
        assert_eq!(
            render_frontmatter_document(simple.as_object().unwrap(), "x"),
            "---\na: 1\nb: two\n---\nx"
        );

        // System keys are never written.
        let with_system = json!({"title": "t", "entry_type": "memory", "created_at": "now"});
        let doc = render_frontmatter_document(with_system.as_object().unwrap(), "");
        assert_eq!(doc, "---\ntitle: t\n---\n");

        // An empty map still produces a recognized (empty) frontmatter block.
        let doc = render_frontmatter_document(&Map::new(), "body\n");
        assert_eq!(
            parse_vault_frontmatter_at(Some("kinds/adr/x.md"), &doc),
            ("{}".to_string(), "body\n".to_string())
        );
        assert!(doc.starts_with("---\n"));
    }

    /// The vault files this repository ships and tracks read the same
    /// under both parsers.
    #[test]
    fn repository_vault_files_agree() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [crate_dir.join("../.vault"), crate_dir.join("vault")];
        let mut checked = 0;
        for root in &roots {
            for entry in walkdir::WalkDir::new(root) {
                let entry = entry.unwrap();
                if entry.path().extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let content = std::fs::read_to_string(entry.path()).unwrap();
                let Some((yaml, _)) = split_frontmatter(&content) else {
                    continue;
                };
                assert_eq!(
                    parse_frontmatter_strict(yaml).map(Value::Object),
                    Ok(legacy(yaml)),
                    "{}",
                    entry.path().display()
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no vault files found under {roots:?}");
    }
}
