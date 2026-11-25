//! Automatic, bounded source-syntax measurements over before/after snapshots.
//! Inputs are supplied recorded source; this module performs no filesystem I/O.

pub(crate) mod languages;
pub(crate) mod tokens;

use crate::catalog::{MetricScope, METRIC_CARDS};
use crate::metrics::{
    ExtractedMetric, Measurement, Metric, MetricLocation, MetricObservation, EXTRACTION_CONVENTION,
};
use atomic_semantic::{Language, ParserRegistry, SyntaxNode as Node};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub enum Source {
    Text(String),
    /// Proven absence, not a failed read or an unavailable historical baseline.
    Absent,
    Unavailable(String),
}

impl Source {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        if bytes.contains(&0) {
            return Self::Unavailable("Binary source".into());
        }
        String::from_utf8(bytes)
            .map(Self::Text)
            .unwrap_or_else(|_| Self::Unavailable("Non-UTF-8 source".into()))
    }
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    pub before: Source,
    pub after: Source,
}

#[derive(Clone, Debug)]
pub struct ExtractionConfig {
    pub max_files: usize,
    pub max_total_bytes: usize,
    pub max_tokens_per_file: usize,
    pub max_total_tokens: usize,
    pub max_subjects_per_file: usize,
    pub max_total_subjects: usize,
    pub minimum_clone_tokens: usize,
    pub max_diff_work: usize,
    pub max_clone_pairs: usize,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            max_files: 128,
            max_total_bytes: 8 * 1024 * 1024,
            max_tokens_per_file: 20_000,
            max_total_tokens: 80_000,
            max_subjects_per_file: 256,
            max_total_subjects: 1024,
            minimum_clone_tokens: 32,
            max_diff_work: 16_000_000,
            max_clone_pairs: 100_000,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Extraction {
    pub metrics: Vec<ExtractedMetric>,
    pub diagnostics: Vec<String>,
    /// Exact supplied comparison paths, not an assertion of repository coverage.
    pub comparison_paths: Vec<String>,
}

#[derive(Clone, Debug)]
struct Entry {
    value: Measurement,
    location: MetricLocation,
    reliable_identity: bool,
}

#[derive(Clone, Debug)]
enum State {
    Ready,
    Absent,
    Unavailable(String),
}

struct Parsed {
    language: Option<Language>,
    state: State,
    entries: BTreeMap<(Metric, String), Entry>,
    tokens: Vec<tokens::Token>,
    token_error: Option<String>,
    location: Option<MetricLocation>,
}

impl Parsed {
    fn unavailable(language: Option<Language>, reason: impl Into<String>) -> Self {
        Self {
            language,
            state: State::Unavailable(reason.into()),
            entries: BTreeMap::new(),
            tokens: vec![],
            token_error: None,
            location: None,
        }
    }

    fn value(&self, key: &(Metric, String), known_other: bool) -> Measurement {
        if let Some(entry) = self.entries.get(key) {
            return entry.value.clone();
        }
        match &self.state {
            State::Absent => not_applicable("File is absent in this revision"),
            State::Unavailable(reason) => unavailable(reason),
            State::Ready if known_other => {
                not_applicable("Subject is not present in this revision")
            }
            State::Ready => not_applicable("No applicable declarations in this file"),
        }
    }
}

fn measured(value: u32) -> Measurement {
    Measurement::Measured { value }
}
fn unavailable(reason: &str) -> Measurement {
    Measurement::Unavailable {
        reason: reason.into(),
    }
}
fn not_applicable(reason: &str) -> Measurement {
    Measurement::NotApplicable {
        reason: reason.into(),
    }
}

fn location(node: Node<'_>) -> MetricLocation {
    MetricLocation {
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32
            + u32::from(
                node.end_position().column > 0
                    || node.end_position().row == node.start_position().row,
            ),
    }
}

fn metric_scope(metric: Metric) -> MetricScope {
    METRIC_CARDS
        .iter()
        .find(|card| card.metric == metric)
        .expect("catalog covers every metric")
        .scope
}

fn inventory(root: Node<'_>) -> Result<Vec<Node<'_>>, String> {
    let mut result = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 256 {
            return Err("AST nesting budget exceeded".into());
        }
        if tokens::comment(node) {
            continue;
        }
        result.push(node);
        if result.len() > 300_000 {
            return Err("AST node budget exceeded".into());
        }
        stack.extend(
            tokens::named(node)
                .into_iter()
                .rev()
                .map(|child| (child, depth + 1)),
        );
    }
    Ok(result)
}

fn parse(
    source: &Source,
    path: &str,
    registry: &mut ParserRegistry,
    config: &ExtractionConfig,
    bytes_left: &mut usize,
    tokens_left: &mut usize,
    subjects_left: &mut usize,
) -> Parsed {
    let language = Language::detect(path);
    let source = match source {
        Source::Absent => {
            return Parsed {
                language,
                state: State::Absent,
                entries: BTreeMap::new(),
                tokens: vec![],
                token_error: None,
                location: None,
            }
        }
        Source::Unavailable(reason) => return Parsed::unavailable(language, reason),
        Source::Text(source) => source,
    };
    let Some(language) = language else {
        return Parsed::unavailable(None, "Unsupported language");
    };
    if source.len() > atomic_semantic::MAX_SOURCE_BYTES || source.len() > *bytes_left {
        return Parsed::unavailable(Some(language), "Source byte budget exceeded");
    }
    *bytes_left -= source.len();
    if source.contains('\0') {
        return Parsed::unavailable(Some(language), "Binary source");
    }
    let Some(tree) = registry.parse_tree(path, source) else {
        return Parsed::unavailable(Some(language), "No syntax tree");
    };
    if tree.root_node().has_error() {
        return Parsed::unavailable(
            Some(language),
            "Syntax recovery prevents reliable metric extraction",
        );
    }
    let nodes = match inventory(tree.root_node()) {
        Ok(nodes) => nodes,
        Err(reason) => return Parsed::unavailable(Some(language), reason),
    };
    let token_limit = config.max_tokens_per_file.min(*tokens_left);
    let (token_stream, token_error) =
        match tokens::leaf_tokens(tree.root_node(), source, token_limit) {
            Ok(tokens) => (tokens, None),
            Err(reason) => {
                *tokens_left -= token_limit;
                (Vec::new(), Some(reason))
            }
        };
    *tokens_left -= token_stream.len();
    let functions: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| languages::callable(*node, language))
        .collect();
    let classes: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| languages::class_like(*node, language))
        .collect();
    if functions.len() + classes.len() > config.max_subjects_per_file
        || functions.len() + classes.len() > *subjects_left
    {
        return Parsed::unavailable(Some(language), "Subject inventory budget exceeded");
    }
    *subjects_left -= functions.len() + classes.len();
    let mut entries = BTreeMap::new();
    let mut names = BTreeMap::<String, usize>::new();
    for &node in functions.iter().chain(classes.iter()) {
        *names
            .entry(languages::qualified(node, source, language).0)
            .or_default() += 1;
    }
    let mut ordinals = BTreeMap::<String, usize>::new();
    for &node in &functions {
        let (name, stable) = languages::qualified(node, source, language);
        let ordinal = ordinals.entry(name.clone()).or_default();
        *ordinal += 1;
        let subject = format!("{path}#function:{name}:{}", *ordinal);
        let reliable = stable && names[&name] == 1;
        let body = languages::body(node, language);
        let body_missing = if languages::body_is_missing(node, source, language) {
            unavailable("Implementation body is outside this source; not an abstract-body zero")
        } else {
            not_applicable("Declaration has no executable source body")
        };
        let (complexity, depth) = body
            .map(|body| languages::control_flow(body, source, language))
            .unwrap_or((0, 0));
        for (metric, value) in [
            (
                Metric::MethodLength,
                body.map(|body| measured(tokens::code_lines(body, source, language, true)))
                    .unwrap_or_else(|| body_missing.clone()),
            ),
            (
                Metric::ParameterCount,
                languages::parameters(node, source, language),
            ),
            (
                Metric::NestingDepth,
                body.map(|_| measured(depth))
                    .unwrap_or_else(|| body_missing.clone()),
            ),
            (
                Metric::CyclomaticComplexity,
                body.map(|_| measured(complexity))
                    .unwrap_or_else(|| body_missing.clone()),
            ),
        ] {
            entries.insert(
                (metric, subject.clone()),
                Entry {
                    value,
                    location: location(node),
                    reliable_identity: reliable,
                },
            );
        }
    }
    for &class in &classes {
        let (name, stable) = languages::qualified(class, source, language);
        let ordinal = ordinals.entry(name.clone()).or_default();
        *ordinal += 1;
        let subject = format!("{path}#class:{name}:{}", *ordinal);
        let Some(body) = languages::body(class, language) else {
            for metric in [
                Metric::ClassLength,
                Metric::MethodCount,
                Metric::ClassComplexity,
            ] {
                entries.insert(
                    (metric, subject.clone()),
                    Entry {
                        value: unavailable("Class body is only forward-declared in this source"),
                        location: location(class),
                        reliable_identity: stable && names[&name] == 1,
                    },
                );
            }
            continue;
        };
        let methods: Vec<_> = functions
            .iter()
            .copied()
            .filter(|node| {
                languages::method(*node, language)
                    && languages::owner_class(*node, language)
                        .is_some_and(|owner| owner.id() == class.id())
            })
            .collect();
        let unresolved = methods
            .iter()
            .any(|method| languages::body_is_missing(*method, source, language));
        let complexity = if unresolved {
            unavailable("One or more method implementations are outside this source; aggregate is incomplete")
        } else {
            measured(
                methods
                    .iter()
                    .filter_map(|method| languages::body(*method, language))
                    .map(|body| languages::control_flow(body, source, language).0)
                    .sum(),
            )
        };
        for (metric, value) in [
            (
                Metric::ClassLength,
                measured(tokens::code_lines(body, source, language, false)),
            ),
            (Metric::MethodCount, measured(methods.len() as u32)),
            (Metric::ClassComplexity, complexity),
        ] {
            entries.insert(
                (metric, subject.clone()),
                Entry {
                    value,
                    location: location(class),
                    reliable_identity: stable && names[&name] == 1,
                },
            );
        }
    }
    for (metric, value) in [
        (
            Metric::DependencyFanOut,
            languages::dependencies(&nodes, source, language),
        ),
        (
            Metric::PublicSurface,
            languages::public_surface(&nodes, source, language),
        ),
    ] {
        entries.insert(
            (metric, path.into()),
            Entry {
                value,
                location: location(tree.root_node()),
                reliable_identity: true,
            },
        );
    }
    Parsed {
        language: Some(language),
        state: State::Ready,
        entries,
        tokens: token_stream,
        token_error,
        location: Some(location(tree.root_node())),
    }
}

fn clone_measurements(
    parsed: &mut [Parsed],
    masks: &[Result<Vec<bool>, String>],
    paths: &[String],
    config: &ExtractionConfig,
    pairs_left: &mut usize,
) {
    let languages: BTreeSet<_> = parsed
        .iter()
        .filter_map(|file| file.language.map(|language| language.name()))
        .collect();
    for language in languages {
        let indices: Vec<_> = parsed
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                file.language.is_some_and(|item| item.name() == language)
                    && matches!(file.state, State::Ready)
            })
            .map(|(index, _)| index)
            .collect();
        let incomplete = parsed.iter().any(|file| {
            file.language.is_some_and(|item| item.name() == language)
                && (matches!(file.state, State::Unavailable(_)) || file.token_error.is_some())
        });
        let matches = if incomplete {
            Err("Same-language comparison corpus is incomplete".into())
        } else {
            let corpus: Vec<_> = indices
                .iter()
                .map(|index| (language, parsed[*index].tokens.as_slice()))
                .collect();
            tokens::duplicated(&corpus, config.minimum_clone_tokens, pairs_left)
        };
        for (position, index) in indices.into_iter().enumerate() {
            let value = match (&matches, &masks[index]) {
                (Ok(matches), Ok(changed)) => measured(
                    matches[position]
                        .iter()
                        .zip(changed)
                        .filter(|(duplicate, changed)| **duplicate && **changed)
                        .count() as u32,
                ),
                (Err(reason), _) | (_, Err(reason)) => unavailable(reason),
            };
            parsed[index].entries.insert(
                (Metric::DuplicatedTokens, paths[index].clone()),
                Entry {
                    value,
                    location: parsed[index].location.clone().unwrap(),
                    reliable_identity: true,
                },
            );
        }
    }
}

/// Extract every catalog metric. Function/class facts are per declared subject;
/// absent constructs are not applicable. Clone scope is exactly the supplied
/// same-language files, with exact tokens and an explicit minimum window size.
/// Unchanged comparison files may be supplied with identical before/after text.
pub fn extract_changes(
    changes: &[FileChange],
    registry: &mut ParserRegistry,
    config: &ExtractionConfig,
) -> Result<Extraction, &'static str> {
    if config.minimum_clone_tokens < 4 || config.minimum_clone_tokens > config.max_tokens_per_file {
        return Err("clone token minimum must be between 4 and the per-file token budget");
    }
    let mut ordered: Vec<_> = changes.iter().collect();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    if ordered.windows(2).any(|pair| pair[0].path == pair[1].path)
        || ordered.iter().any(|change| change.path.is_empty())
    {
        return Err("metric extraction requires unique nonempty source paths");
    }
    let paths: Vec<_> = ordered.iter().map(|change| change.path.clone()).collect();
    let mut bytes_left = config.max_total_bytes;
    let mut tokens_left = config.max_total_tokens;
    let mut subjects_left = config.max_total_subjects;
    let mut before = Vec::new();
    let mut after = Vec::new();
    for (index, change) in ordered.iter().enumerate() {
        if index >= config.max_files {
            before.push(Parsed::unavailable(
                Language::detect(&change.path),
                "File count budget exceeded",
            ));
            after.push(Parsed::unavailable(
                Language::detect(&change.path),
                "File count budget exceeded",
            ));
        } else {
            before.push(parse(
                &change.before,
                &change.path,
                registry,
                config,
                &mut bytes_left,
                &mut tokens_left,
                &mut subjects_left,
            ));
            after.push(parse(
                &change.after,
                &change.path,
                registry,
                config,
                &mut bytes_left,
                &mut tokens_left,
                &mut subjects_left,
            ));
        }
    }
    let mut diff_left = config.max_diff_work;
    let mut old_masks = Vec::new();
    let mut new_masks = Vec::new();
    for (old, new) in before.iter().zip(&after) {
        let masks = if old.token_error.is_some() || new.token_error.is_some() {
            Err("Changed-token comparison lacks a complete token stream".into())
        } else {
            match (&old.state, &new.state) {
                (State::Ready, State::Ready) => {
                    tokens::changed(&old.tokens, &new.tokens, &mut diff_left)
                }
                (State::Ready, State::Absent)
                | (State::Absent, State::Ready)
                | (State::Absent, State::Absent) => {
                    Ok((vec![true; old.tokens.len()], vec![true; new.tokens.len()]))
                }
                _ => Err("Changed-token baseline is unavailable".into()),
            }
        };
        match masks {
            Ok((old, new)) => {
                old_masks.push(Ok(old));
                new_masks.push(Ok(new));
            }
            Err(reason) => {
                old_masks.push(Err(reason.clone()));
                new_masks.push(Err(reason));
            }
        }
    }
    let mut pairs_left = config.max_clone_pairs;
    clone_measurements(&mut before, &old_masks, &paths, config, &mut pairs_left);
    clone_measurements(&mut after, &new_masks, &paths, config, &mut pairs_left);
    let mut result = Extraction {
        metrics: Vec::new(),
        diagnostics: Vec::new(),
        comparison_paths: paths.clone(),
    };
    for (index, (old, new)) in before.iter().zip(&after).enumerate() {
        for (side, file) in [("before", old), ("after", new)] {
            if let State::Unavailable(reason) = &file.state {
                result
                    .diagnostics
                    .push(format!("{} ({side}): {reason}", paths[index]));
            }
            if let Some(reason) = &file.token_error {
                result
                    .diagnostics
                    .push(format!("{} ({side} tokens): {reason}", paths[index]));
            }
        }
        for metric in Metric::ALL {
            let mut subjects: BTreeSet<_> = old
                .entries
                .keys()
                .chain(new.entries.keys())
                .filter(|(item, _)| *item == metric)
                .map(|(_, subject)| subject.clone())
                .collect();
            if subjects.is_empty() {
                subjects.insert(format!("{}#<{:?}>", paths[index], metric_scope(metric)));
            }
            for subject in subjects {
                let key = (metric, subject.clone());
                let old_entry = old.entries.get(&key);
                let new_entry = new.entries.get(&key);
                let mut old_value = old.value(&key, new_entry.is_some());
                let mut new_value = new.value(&key, old_entry.is_some());
                let ambiguous = matches!((&old.state, &new.state), (State::Ready, State::Ready))
                    && old_entry
                        .into_iter()
                        .chain(new_entry)
                        .any(|entry| !entry.reliable_identity);
                if ambiguous {
                    old_value = unavailable(
                        "Anonymous/overloaded subject matching is ambiguous; no delta inferred",
                    );
                    if new_entry.is_none() {
                        new_value =
                            unavailable("Anonymous/overloaded subject matching is ambiguous");
                    }
                }
                let detail = if metric == Metric::DuplicatedTokens {
                    format!("Exact same-language tokens in {} supplied comparison files; minimum {} tokens; counts only changed token positions, without overlapping-position double counts", paths.len(), config.minimum_clone_tokens)
                } else {
                    "Source-syntax convention v1; not compiler-resolved runtime behavior. Compare only matching named subjects and counting conventions".into()
                };
                let observation = MetricObservation {
                    metric,
                    subject,
                    before: old_value,
                    after: new_value,
                };
                result.metrics.push(ExtractedMetric {
                    delta: observation.delta(),
                    observation,
                    path: paths[index].clone(),
                    scope: metric_scope(metric),
                    subject_identity_ambiguous: old_entry
                        .into_iter()
                        .chain(new_entry)
                        .any(|entry| !entry.reliable_identity),
                    before_location: if ambiguous {
                        None
                    } else {
                        old_entry.map(|entry| entry.location.clone())
                    },
                    after_location: new_entry.map(|entry| entry.location.clone()),
                    convention: EXTRACTION_CONVENTION.into(),
                    detail,
                });
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
