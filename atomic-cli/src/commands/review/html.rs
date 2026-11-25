//! Offline review artifact. All source reads are pinned to the selected change,
//! and exported text is data, never trusted HTML or executable JavaScript.

use super::{incremental::IncrementalReport, presentation};
use crate::error::{CliError, CliResult};
use atomic_canonical::review_finding::SourceSide;
use atomic_canonical::review_graph::ReviewGraph;
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SourceText {
    Present { text: String },
    Absent,
    Unavailable { reason: String },
}

impl SourceText {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Snapshot {
    change: String,
    path: String,
    reviewed_root: bool,
    before: SourceText,
    after: SourceText,
}

#[derive(Default)]
struct Request {
    known: BTreeSet<String>,
    absent: BTreeSet<String>,
    findings: bool,
}

pub(super) fn render(repo: &Repository, report: &IncrementalReport) -> CliResult<String> {
    super::view_snapshot::verify(repo, report)?;
    let graph = report.graph.as_ref().expect("resolved review graph");
    let regions = presentation::regions(graph);
    let sources = sources(repo, report, &regions);
    super::view_snapshot::verify(repo, report)?;
    let patterns: BTreeMap<_, _> = atomic_audit::structural::Rule::ALL
        .into_iter()
        .map(|rule| (rule.id(), rule.coverage()))
        .collect();
    // Archive exactly the same bounded report as --json, without a JavaScript
    // number roundtrip. Dependency-only changes carry references, not patches.
    let mut archive = std::io::BufWriter::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    serde_json::to_writer(&mut archive, report)
        .map_err(|error| CliError::Internal(error.into()))?;
    let archive = archive
        .into_inner()
        .map_err(|error| CliError::Internal(error.into()))?
        .finish()?;
    let archive = data_encoding::BASE64.encode(&archive);
    let view = json!({
        "selected_roots":report.selected_roots,"warnings":report.warnings,
        "view_snapshot":report.view_snapshot.as_ref().map(|snapshot|json!({"source_ref":snapshot.source_ref,"view":snapshot.view,"state":snapshot.state,"membership_hash":snapshot.membership_hash,"file_count":snapshot.paths.len(),"conformance":snapshot.conformance})),
        "view_comparison":report.view_comparison.as_ref().map(|comparison|json!({"source_ref":comparison.source_ref,"from":comparison.from,"to":comparison.to,"diagnostics":comparison.diagnostics,"conformance":comparison.conformance,"files":comparison.files.iter().map(|file|json!({"source_ref":file.source_ref,"path":file.path,"before_path":file.before_path,"after_path":file.after_path,"status":file.status,"content_status":file.content_status})).collect::<Vec<_>>()})),
        "changes":report.changes.iter().filter(|change|change.selected_root).map(|change|json!({"hash":change.hash,"diff":change.diff,"diff_status":change.diff_status})).collect::<Vec<_>>(),
        "graph":{"scope":graph.scope,"passes":graph.passes,"findings":graph.findings,"criteria":graph.criteria,"diagnostics":graph.diagnostics,"risk":graph.risk,
            "nodes":graph.nodes.iter().filter(|node|matches!(node.kind.as_str(),"metric"|"check")).collect::<Vec<_>>()}
    });
    let profile_json = graph
        .risk
        .as_ref()
        .and_then(|risk| risk.model.as_ref())
        .map(|model| serde_json::to_string_pretty(&model.profile))
        .transpose()
        .map_err(|error| CliError::Internal(error.into()))?;
    emit(
        json!({"schema":"atomic.review-viewer.v1","local_only":true,"view":view,"report_archive":{"encoding":"gzip+base64","data":archive},"profile_json":profile_json,"regions":regions,"sources":sources,"audit_cards":atomic_audit::AUDIT_CARDS,"metric_cards":atomic_audit::METRIC_CARDS,"patterns":patterns}),
        &super::terminal::render(graph, false),
    )
}

fn collect_requests(
    graph: &ReviewGraph,
    regions: &[presentation::Region],
) -> BTreeMap<(String, String), Request> {
    let mut requests = BTreeMap::<(String, String), Request>::new();
    for region in regions {
        let request = requests
            .entry((region.change.clone(), region.path.clone()))
            .or_default();
        request.findings |= !region.findings.is_empty();
        if !region.findings.is_empty() || !region.impact.is_empty() {
            request.known.insert(region.side.clone());
        }
    }
    for node in graph.nodes.iter().filter(|node| node.kind == "metric") {
        let (Some(change), Some(path)) = (
            node.properties["change"].as_str(),
            node.properties["path"].as_str(),
        ) else {
            continue;
        };
        let request = requests.entry((change.into(), path.into())).or_default();
        for side in ["before", "after"] {
            let value = &node.properties["observation"][side];
            if value["status"] == "measured"
                && presentation::metric_location(node, side).0.is_some()
            {
                request.known.insert(side.into());
            }
            if value["status"] == "not_applicable"
                && value["reason"] == "File is absent in this revision"
            {
                request.absent.insert(side.into());
            }
        }
    }
    // Explicit source locations also cover supplied findings without metrics.
    for finding in &graph.findings {
        for location in &finding.finding.locations {
            requests
                .entry((finding.finding.change.clone(), location.path.clone()))
                .or_default()
                .known
                .insert(
                    match location.side {
                        SourceSide::Before => "before",
                        SourceSide::After => "after",
                    }
                    .into(),
                );
        }
    }
    requests
}

fn sources(
    repo: &Repository,
    report: &IncrementalReport,
    regions: &[presentation::Region],
) -> Vec<Snapshot> {
    let graph = report.graph.as_ref().expect("resolved review graph");
    let mut requests = collect_requests(graph, regions)
        .into_iter()
        .collect::<Vec<_>>();
    let reviewed: BTreeSet<_> = report
        .changes
        .iter()
        .filter(|change| change.selected_root)
        .flat_map(|change| {
            change
                .paths
                .iter()
                .map(|path| (change.hash.clone(), path.clone()))
        })
        .chain(report.view_snapshot.iter().flat_map(|snapshot| {
            snapshot
                .paths
                .iter()
                .map(|path| (snapshot.source_ref.clone(), path.clone()))
        }))
        .chain(report.view_comparison.iter().flat_map(|comparison| {
            comparison.files.iter().flat_map(|file| {
                std::iter::once(file.path.as_str())
                    .chain(file.before_path.as_deref())
                    .chain(file.after_path.as_deref())
                    .map(|path| (file.source_ref.clone(), path.to_owned()))
            })
        }))
        .chain(report.view_comparison.iter().flat_map(|comparison| {
            comparison
                .files
                .iter()
                .flat_map(|file| file.before_path.iter().chain(&file.after_path))
                .map(|path| (comparison.source_ref.clone(), path.clone()))
        }))
        .collect();
    requests.sort_by(|(left, a), (right, b)| {
        b.findings
            .cmp(&a.findings)
            .then(reviewed.contains(right).cmp(&reviewed.contains(left)))
            .then(b.known.len().cmp(&a.known.len()))
            .then(left.cmp(right))
    });
    let config = atomic_audit::extract::ExtractionConfig::default();
    let mut bytes = config.max_total_bytes;
    let mut files = 0;
    let mut result = Vec::new();
    for ((change, path), request) in requests {
        let reviewed_root = reviewed.contains(&(change.clone(), path.clone()));
        let comparison_file = report
            .view_comparison
            .as_ref()
            .and_then(|comparison| comparison.file(&change));
        let comparable_bytes = comparison_file
            .is_some_and(|file| file.before_hash.is_some() || file.after_hash.is_some());
        // References and unavailable observations consume no source-read budget.
        let limited = if !reviewed_root || (request.known.is_empty() && !comparable_bytes) {
            false
        } else if files >= config.max_files {
            true
        } else {
            files += 1;
            false
        };
        let on_view = report
            .changes
            .iter()
            .any(|item| item.hash == change && item.selected_root && item.on_current_view);
        let hash = Hash::from_base32(change.as_bytes());
        let mut load = |side: &str| {
            if let Some(comparison) = report
                .view_comparison
                .as_ref()
                .filter(|comparison| comparison.source_ref == change)
            {
                if atomic_repository::is_vault_path(&path) {
                    return SourceText::unavailable(
                        "Vault definitions are conformance evidence, not code-source export",
                    );
                }
                let pin = if side == "before" {
                    &comparison.from
                } else {
                    &comparison.to
                };
                let belongs = comparison.files.iter().any(|file| {
                    if side == "before" {
                        file.before_path.as_ref() == Some(&path)
                    } else {
                        file.after_path.as_ref() == Some(&path)
                    }
                });
                if !belongs {
                    return SourceText::Absent;
                }
                let inspected = comparison.files.iter().any(|file| {
                    if side == "before" {
                        file.before_path.as_ref() == Some(&path) && file.before_hash.is_some()
                    } else {
                        file.after_path.as_ref() == Some(&path) && file.after_hash.is_some()
                    }
                });
                if !inspected {
                    return SourceText::unavailable("This recorded side was unavailable, non-regular or outside the source inspection budget");
                }
                if limited || bytes == 0 {
                    return SourceText::unavailable("HTML source export budget exceeded; evidence and every finding remain in the report");
                }
                return source_text(repo.get_file_content_on_view(&path, &pin.view), &mut bytes);
            }
            if let (Some(comparison), Some(file)) = (&report.view_comparison, comparison_file) {
                let (actual, pin, hash) = if side == "before" {
                    (&file.before_path, &comparison.from, &file.before_hash)
                } else {
                    (&file.after_path, &comparison.to, &file.after_hash)
                };
                let Some(actual) = actual else {
                    return SourceText::Absent;
                };
                if atomic_repository::is_vault_path(actual) {
                    return SourceText::unavailable(
                        "Vault metadata is excluded from code-source export",
                    );
                }
                if !request.known.contains(side) && hash.is_none() {
                    return SourceText::unavailable("This recorded comparison side was not reliably inspected; no working-copy fallback");
                }
                if limited || bytes == 0 {
                    return SourceText::unavailable(
                        "HTML source export budget exceeded; source is not silently truncated",
                    );
                }
                return source_text(repo.get_file_content_on_view(actual, &pin.view), &mut bytes);
            }
            if report.view_snapshot.is_some() && side == "before" {
                return SourceText::unavailable(super::analysis::SNAPSHOT_BASELINE);
            }
            if !reviewed_root {
                return SourceText::unavailable("Caller context is a bounded recorded reference, not a reviewed path; full caller source is excluded from export");
            }
            if atomic_repository::is_vault_path(&path) {
                return SourceText::unavailable("Vault metadata is excluded from code-source export; intent and recorded diff evidence remain in the report");
            }
            if request.absent.contains(side) && !request.known.contains(side) {
                return SourceText::Absent;
            }
            if !request.known.contains(side) {
                return SourceText::unavailable("Source was unavailable or not inspected for this recorded side; no working-copy fallback");
            }
            if limited || bytes == 0 {
                return SourceText::unavailable(
                    "HTML source export budget exceeded; source is not silently truncated",
                );
            }
            if let Some(snapshot) = &report.view_snapshot {
                if snapshot.source_ref != change {
                    return SourceText::unavailable(
                        "Source does not belong to the pinned view snapshot",
                    );
                }
                return source_text(
                    repo.get_file_content_on_view(&path, &snapshot.view),
                    &mut bytes,
                );
            }
            if !on_view {
                return SourceText::unavailable(
                    "Exact recorded snapshot is off the selected source view",
                );
            }
            let Some(hash) = &hash else {
                return SourceText::unavailable("Invalid recorded change reference");
            };
            read_source(repo, hash, &path, side, &mut bytes)
        };
        // Most concern anchors are after-side; preserve their source first when
        // the explicit total budget cannot hold both snapshots.
        let after = load("after");
        let before = load("before");
        result.push(Snapshot {
            change,
            path,
            reviewed_root,
            before,
            after,
        });
    }
    result
}

fn read_source(
    repo: &Repository,
    hash: &Hash,
    path: &str,
    side: &str,
    bytes: &mut usize,
) -> SourceText {
    let result = if side == "before" {
        repo.get_file_content_before_change(path, hash)
    } else {
        repo.get_file_content_after_change(path, hash)
    };
    source_text(result, bytes)
}

fn source_text(
    result: Result<Option<Vec<u8>>, atomic_repository::RepositoryError>,
    bytes: &mut usize,
) -> SourceText {
    match result {
        Ok(Some(content))
            if content.len() <= atomic_semantic::MAX_SOURCE_BYTES && content.len() <= *bytes =>
        {
            if content.contains(&0) {
                return SourceText::unavailable("Recorded content is binary");
            }
            match String::from_utf8(content) {
                Ok(text) => {
                    *bytes -= text.len();
                    SourceText::Present { text }
                }
                Err(_) => SourceText::unavailable("Recorded content is not UTF-8"),
            }
        }
        Ok(Some(_)) => {
            SourceText::unavailable("Recorded source exceeds the HTML source byte budget")
        }
        Ok(None) => SourceText::unavailable(
            "Recorded source reference could not be materialized; no working-copy fallback",
        ),
        Err(error) => SourceText::unavailable(format!("Recorded source unavailable: {error}")),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn emit(document: Value, fallback: &str) -> CliResult<String> {
    let data = serde_json::to_string(&document)
        .map_err(|error| CliError::Internal(error.into()))?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    Ok(format!(
        include_str!("viewer.html"),
        nonce = nonce,
        logo = data_encoding::BASE64.encode(include_bytes!("assets/atomic-logo.png")),
        css = include_str!("viewer.css"),
        script = include_str!("viewer.js"),
        data = data,
        fallback = escape(fallback)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_and_metadata_cannot_break_out_of_the_embedded_document() {
        let hostile = "</script><img src=x onerror=alert(1)>&\u{2028}\u{2029}";
        let html = emit(json!({"hostile":hostile}), hostile).unwrap();
        assert!(!html.contains(hostile));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("\\u003c/script\\u003e"));
        assert!(html.contains("&lt;/script&gt;"));
        let data = html
            .split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(data).unwrap()["hostile"],
            hostile
        );
        assert!(html.contains("default-src 'none'") && html.contains("connect-src 'none'"));
        assert!(!html.contains("<script src=") && !html.contains("innerHTML"));
    }

    #[test]
    fn original_json_text_remains_exact_for_javascript_downloads() {
        let original = "{\"id\":18446744073709551615,\"source\":\"</script>\"}";
        use std::io::{Read, Write};
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(original.as_bytes()).unwrap();
        let data = data_encoding::BASE64.encode(&encoder.finish().unwrap());
        let html = emit(json!({"report_archive":{"data":data}}), "").unwrap();
        let encoded = html
            .split("id=\"review-data\"")
            .nth(1)
            .unwrap()
            .split_once('>')
            .unwrap()
            .1
            .split("</script>")
            .next()
            .unwrap();
        let document: Value = serde_json::from_str(encoded).unwrap();
        let compressed = data_encoding::BASE64
            .decode(
                document["report_archive"]["data"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let mut restored = String::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut restored)
            .unwrap();
        assert_eq!(restored, original);
        assert!(include_str!("viewer.js").contains("new DecompressionStream('gzip')"));
    }
}
