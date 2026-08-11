//! The bounded CLI skin for a [`TriageReport`].
//!
//! Size lives behind drill-down: a 400-change review yields the same three-line
//! verdict as a 4-change one. This renderer prints a verdict banner, a summary
//! line, the severity-sorted findings, and a compact per-intent view — never a
//! diff dump.

use serde_json::Value;

use atomic_canonical::proof::attest_value;

use crate::commands::provenance::command::resolve_person;
use crate::error::CliResult;
use crate::output::{emphasis, hint, info};

use super::model::{
    ChangeReport, CriterionReport, Finding, IntentReport, TriageReport, Verdict, SEV_BLOCK,
    SEV_INFO, SEV_WARN,
};

/// Render the report to stdout as the bounded human dashboard.
pub fn print_report(report: &TriageReport) {
    let merkle_short: String = report.inputs.view_merkle.chars().take(8).collect();

    // Verdict banner.
    let verdict_label = match report.verdict {
        Verdict::Ready => info("READY").to_string(),
        Verdict::Blocked => emphasis(crate::output::error("BLOCKED")).to_string(),
        Verdict::Stale => crate::output::warning("STALE").to_string(),
    };
    println!(
        "{} {} \u{2192} {}   {}   ({}, {} change{})",
        emphasis("triage"),
        report.inputs.feature,
        report.inputs.target,
        verdict_label,
        hint(&format!("merkle {merkle_short}")),
        report.summary.changes,
        if report.summary.changes == 1 { "" } else { "s" },
    );

    // Summary line.
    println!(
        "  criteria {} met \u{00b7} {} unmet    findings {} block \u{00b7} {} warn \u{00b7} {} info",
        report.summary.criteria_met,
        report.summary.criteria_unmet,
        report.summary.findings_block,
        report.summary.findings_warn,
        report.summary.findings_info,
    );

    // Findings (already severity-sorted block → warn → info).
    if !report.findings.is_empty() {
        println!("\n{}", emphasis("Findings"));
        for f in &report.findings {
            println!("  {}", finding_line(f));
        }
    }

    // Per-intent compact view with its criteria as ✓/✗.
    if !report.intents.is_empty() {
        println!("\n{}", emphasis("Intents"));
        for intent in &report.intents {
            let conform = if intent.conforms {
                info("conforms").to_string()
            } else {
                crate::output::error(format!("{} violation(s)", intent.gate_violations.len()))
                    .to_string()
            };
            println!("  {} [{}]", emphasis(&intent.id), conform);
            if let Some(reviewer) = &intent.reviewed_by {
                println!("    {}", info(&format!("reviewed by {reviewer}")));
            }
            for c in &intent.criteria {
                let mark = if c.status == "met" {
                    info("\u{2713}").to_string()
                } else {
                    crate::output::error("\u{2717}").to_string()
                };
                let judged = if c.judgment_required {
                    hint(" (judgment required)").to_string()
                } else {
                    String::new()
                };
                println!(
                    "    {} {} {}{}",
                    mark,
                    hint(&c.id),
                    first_line(&c.text),
                    judged
                );
            }
        }
    }

    // Per-change review context, bounded: one head line + the first line of the
    // commit message + aggregate +/- stats + the exact command to see the code
    // (never a hunk dump — the full inline diff is the HTML skin's job).
    if !report.changes.is_empty() {
        println!("\n{}", emphasis("Changes"));
        for c in &report.changes {
            let short: String = c.id.chars().take(12).collect();
            let (adds, dels) = diff_stat_counts(c);
            let stat = if c.diff.is_empty() {
                String::new()
            } else {
                format!(
                    "  {}",
                    hint(&format!("+{adds} -{dels} across {} file(s)", c.diff.len()))
                )
            };
            println!("  {} [{}]{}", emphasis(&short), hint(&c.coverage), stat);
            if !c.message.is_empty() {
                println!("    {}", first_line(&c.message));
            }
            println!("    {}", hint(&c.review_command));
        }
    }

    // Drill-down pointer (JSON is the full worklist; keep the CLI bounded).
    println!(
        "\n{}",
        hint("run with --json for the full worklist (findings, criteria, provenance)")
    );
    println!("{} {}", hint("ref"), hint(&report.reference));
}

/// Aggregate (additions, deletions) across a change's embedded diff.
fn diff_stat_counts(c: &ChangeReport) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for f in &c.diff {
        for h in &f.hunks {
            for l in &h.lines {
                match l.tag.as_str() {
                    "+" => adds += 1,
                    "-" => dels += 1,
                    _ => {}
                }
            }
        }
    }
    (adds, dels)
}

/// One severity-tagged finding line.
fn finding_line(f: &Finding) -> String {
    let sev = match f.severity.as_str() {
        SEV_BLOCK => crate::output::error("block").to_string(),
        SEV_WARN => crate::output::warning("warn").to_string(),
        SEV_INFO => info("info").to_string(),
        other => other.to_string(),
    };
    format!("[{}] {} {}", sev, emphasis(&f.code), f.message)
}

/// The first line of a possibly multi-line string, for bounded output.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

// ── Attested export skin ────────────────────────────────────────────────────

/// Sign the report into an attested, frozen JSON artifact.
///
/// Serializes the report to a JSON object, resolves the person's signing
/// identity + keypair (the same path `provenance show --sign` uses), and
/// attaches an Ed25519 Data Integrity proof via the shared `eddsa-jcs-2022`
/// signer. The returned value is the report Value plus `attributedTo`,
/// `contentHash`, and `proof` — verifiable with
/// [`atomic_canonical::proof::verify_value`].
pub fn attest_report(report: &TriageReport, identity: Option<&str>) -> CliResult<Value> {
    let (identity, keypair) = resolve_person(identity)?;
    let value = serde_json::to_value(report).expect("triage report serialization is infallible");
    Ok(attest_value(value, &identity, &keypair))
}

// ── HTML skin ───────────────────────────────────────────────────────────────

/// Render the report as ONE self-contained HTML document.
///
/// No external URLs, CDNs, or scripts: all CSS is inline and the only JavaScript
/// is a tiny vanilla filter function. Every interpolated string is HTML-escaped
/// (via [`html_escape`]); the embedded report JSON is `<`/`>`/`&`-escaped so it
/// cannot break out of its `<script>` container.
pub fn render_html(report: &TriageReport) -> String {
    let verdict_class = match report.verdict {
        Verdict::Blocked => "blocked",
        Verdict::Stale => "stale",
        Verdict::Ready => "ready",
    };
    let verdict_label = match report.verdict {
        Verdict::Blocked => "BLOCKED",
        Verdict::Stale => "STALE",
        Verdict::Ready => "READY",
    };
    let merkle_short: String = report.inputs.view_merkle.chars().take(8).collect();

    let mut h = String::new();
    h.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    h.push_str("<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str(&format!(
        "<title>Triage {} \u{2192} {}</title>\n",
        html_escape(&report.inputs.feature),
        html_escape(&report.inputs.target)
    ));
    h.push_str("<style>\n");
    h.push_str(HTML_STYLE);
    h.push_str("\n</style>\n</head>\n<body>\n");

    // Verdict banner.
    h.push_str(&format!("<header class=\"banner {verdict_class}\">\n"));
    h.push_str(&format!(
        "<div class=\"verdict\">{}</div>\n",
        html_escape(verdict_label)
    ));
    h.push_str(&format!(
        "<div class=\"route\">{} \u{2192} {}</div>\n",
        html_escape(&report.inputs.feature),
        html_escape(&report.inputs.target)
    ));
    h.push_str(&format!(
        "<div class=\"merkle\">merkle {}</div>\n",
        html_escape(&merkle_short)
    ));
    let s = &report.summary;
    h.push_str(&format!(
        "<div class=\"counts\">{} changes \u{00b7} {} files \u{00b7} criteria {} met / {} unmet \u{00b7} findings {} block / {} warn / {} info</div>\n",
        s.changes, s.files, s.criteria_met, s.criteria_unmet, s.findings_block, s.findings_warn, s.findings_info
    ));
    h.push_str(&format!(
        "<div class=\"ref\">{}</div>\n",
        html_escape(&report.reference)
    ));
    h.push_str("</header>\n");

    // Findings.
    h.push_str("<section id=\"findings\">\n<h2>Findings</h2>\n");
    h.push_str("<div class=\"filters\">\n");
    for (label, key) in [
        ("All", "all"),
        ("Block", "block"),
        ("Warn", "warn"),
        ("Info", "info"),
    ] {
        let active = if key == "all" { " active" } else { "" };
        h.push_str(&format!(
            "<button class=\"filter-btn{active}\" data-filter=\"{key}\" onclick=\"filterFindings('{key}')\">{label}</button>\n"
        ));
    }
    h.push_str("</div>\n");
    if report.findings.is_empty() {
        h.push_str("<p class=\"empty\">No findings.</p>\n");
    } else {
        for f in &report.findings {
            h.push_str(&render_finding_card(f));
        }
    }
    h.push_str("</section>\n");

    // Intents.
    h.push_str("<section id=\"intents\">\n<h2>Intents</h2>\n");
    if report.intents.is_empty() {
        h.push_str("<p class=\"empty\">No intents reached.</p>\n");
    } else {
        for intent in &report.intents {
            h.push_str(&render_intent(intent));
        }
    }
    h.push_str("</section>\n");

    // Changes.
    h.push_str("<section id=\"changes\">\n<h2>Changes</h2>\n");
    if report.changes.is_empty() {
        h.push_str("<p class=\"empty\">No candidate changes.</p>\n");
    } else {
        for c in &report.changes {
            h.push_str(&render_change(c));
        }
    }
    h.push_str("</section>\n");

    // Embedded report JSON (escaped so it cannot break out of the script tag).
    let json = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
    h.push_str("<script type=\"application/json\" id=\"triage-report\">\n");
    h.push_str(&escape_script_json(&json));
    h.push_str("\n</script>\n");

    // The only script: a tiny vanilla filter toggler.
    h.push_str("<script>\n");
    h.push_str(FILTER_SCRIPT);
    h.push_str("\n</script>\n");

    h.push_str("</body>\n</html>\n");
    h
}

fn render_finding_card(f: &Finding) -> String {
    let sev = match f.severity.as_str() {
        SEV_BLOCK => "block",
        SEV_WARN => "warn",
        SEV_INFO => "info",
        _ => "info",
    };
    let mut card = String::new();
    card.push_str(&format!(
        "<div class=\"finding {sev}\" data-severity=\"{sev}\">\n"
    ));
    card.push_str(&format!(
        "<div class=\"finding-head\"><span class=\"sev-badge {sev}\">{}</span><span class=\"code\">{}</span></div>\n",
        html_escape(sev),
        html_escape(&f.code)
    ));
    card.push_str(&format!(
        "<div class=\"message\">{}</div>\n",
        html_escape(&f.message)
    ));
    card.push_str(&format!(
        "<div class=\"focus\">focus: <code>{}</code></div>\n",
        html_escape(&f.focus)
    ));
    if let Some(q) = &f.suggested_query {
        card.push_str(&format!(
            "<div class=\"query\">query: <code>{}</code></div>\n",
            html_escape(q)
        ));
    }
    if let Some(r) = &f.remedy {
        card.push_str(&format!(
            "<div class=\"remedy\">remedy: {}</div>\n",
            html_escape(r)
        ));
    }
    card.push_str("</div>\n");
    card
}

fn render_intent(intent: &IntentReport) -> String {
    let conform = if intent.conforms {
        "conforms".to_string()
    } else {
        format!("{} violation(s)", intent.gate_violations.len())
    };
    let conform_class = if intent.conforms { "ok" } else { "bad" };
    let mut d = String::new();
    d.push_str("<details class=\"intent\">\n");
    d.push_str(&format!(
        "<summary><span class=\"intent-id\">{}</span> <span class=\"badge {conform_class}\">{}</span></summary>\n",
        html_escape(&intent.id),
        html_escape(&conform)
    ));
    if let Some(reviewer) = &intent.reviewed_by {
        d.push_str(&format!(
            "<div class=\"reviewed-by\">reviewed by {}</div>\n",
            html_escape(reviewer)
        ));
    }
    if let Some(why) = &intent.why {
        d.push_str(&format!("<p class=\"why\">{}</p>\n", html_escape(why)));
    }
    if !intent.gate_violations.is_empty() {
        d.push_str("<ul class=\"violations\">\n");
        for v in &intent.gate_violations {
            d.push_str(&format!("<li>{}</li>\n", html_escape(v)));
        }
        d.push_str("</ul>\n");
    }
    if !intent.criteria.is_empty() {
        d.push_str("<ul class=\"criteria\">\n");
        for c in &intent.criteria {
            d.push_str(&render_criterion(c));
        }
        d.push_str("</ul>\n");
    }
    d.push_str("</details>\n");
    d
}

fn render_criterion(c: &CriterionReport) -> String {
    let met = c.status == "met";
    let mark = if met { "\u{2713}" } else { "\u{2717}" };
    let mark_class = if met { "met" } else { "unmet" };
    let judged = if c.judgment_required {
        "<span class=\"badge judge\">judgment</span>".to_string()
    } else {
        String::new()
    };
    format!(
        "<li class=\"criterion\"><span class=\"mark {mark_class}\">{mark}</span> <span class=\"ac-id\">{}</span> {} {judged}</li>\n",
        html_escape(&c.id),
        html_escape(first_line(&c.text))
    )
}

fn render_change(c: &ChangeReport) -> String {
    let cov = html_escape(&c.coverage);
    let cov_class = match c.coverage.as_str() {
        "covered" => "covered",
        "uncovered" => "uncovered",
        _ => "unknown",
    };
    let (adds, dels) = diff_stat_counts(c);
    let stat = if c.diff.is_empty() {
        String::new()
    } else {
        format!(
            " <span class=\"stat\">+{adds} -{dels} \u{00b7} {} file(s)</span>",
            c.diff.len()
        )
    };
    let mut s = String::new();
    // Collapsible, like intents — the summary conveys the change's shape while
    // collapsed; the (potentially long) diff lives in the body.
    s.push_str("<details class=\"change\">\n");
    s.push_str(&format!(
        "<summary><code>{}</code> <span class=\"badge cov-{cov_class}\">{cov}</span>{stat}</summary>\n",
        html_escape(&c.id)
    ));
    if !c.message.is_empty() {
        s.push_str(&format!(
            "<div class=\"change-msg\">{}</div>\n",
            html_escape(&c.message)
        ));
    }
    // Changed files: symbol + path + per-file hunk summary (no diff dump).
    if !c.files.is_empty() {
        s.push_str("<table class=\"files\">\n");
        for f in &c.files {
            s.push_str(&format!(
                "<tr><td class=\"sym\">{}</td><td><code>{}</code></td><td class=\"summary\">{}</td></tr>\n",
                html_escape(&f.symbol),
                html_escape(&f.path),
                html_escape(&f.summary)
            ));
        }
        s.push_str("</table>\n");
    } else if !c.modifies.is_empty() {
        // Fallback (change not loadable): show the modified file-node ids.
        s.push_str("<ul class=\"modifies\">\n");
        for m in &c.modifies {
            s.push_str(&format!("<li><code>{}</code></li>\n", html_escape(m)));
        }
        s.push_str("</ul>\n");
    }
    s.push_str(&format!(
        "<div class=\"review-cmd\">inspect: <code>{}</code></div>\n",
        html_escape(&c.review_command)
    ));
    // The real code-review surface: inline, color-coded unified diff.
    for f in &c.diff {
        s.push_str(&format!(
            "<div class=\"diff-file\"><span class=\"diff-path\">{}</span> <span class=\"diff-status {}\">{}</span></div>\n",
            html_escape(&f.path),
            html_escape(&f.status),
            html_escape(&f.status)
        ));
        s.push_str("<pre class=\"diff\">");
        for hunk in &f.hunks {
            s.push_str(&format!(
                "<div class=\"hunk-header\">{}</div>",
                html_escape(&hunk.header)
            ));
            for line in &hunk.lines {
                let cls = match line.tag.as_str() {
                    "+" => "add",
                    "-" => "del",
                    _ => "ctx",
                };
                s.push_str(&format!(
                    "<div class=\"line {cls}\">{}{}</div>",
                    html_escape(&line.tag),
                    html_escape(&line.content)
                ));
            }
        }
        s.push_str("</pre>\n");
    }
    if !c.blast_radius.is_empty() {
        s.push_str(&format!(
            "<div class=\"blast\">blast radius ({}):</div>\n<ul class=\"blast-list\">\n",
            c.blast_radius.len()
        ));
        for caller in &c.blast_radius {
            s.push_str(&format!("<li><code>{}</code></li>\n", html_escape(caller)));
        }
        s.push_str("</ul>\n");
    }
    s.push_str("</details>\n");
    s
}

/// HTML-escape the five significant characters so no interpolated string can
/// inject markup.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escape a JSON payload for safe embedding inside a `<script>` element: the
/// `<`, `>`, and `&` bytes are rewritten to their `\u00XX` JSON escapes so the
/// literal sequence `</script>` (and `<!--`) can never terminate the tag.
fn escape_script_json(s: &str) -> String {
    s.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

const FILTER_SCRIPT: &str = r#"function filterFindings(sev){
  document.querySelectorAll('.finding').forEach(function(el){
    el.style.display = (sev === 'all' || el.getAttribute('data-severity') === sev) ? '' : 'none';
  });
  document.querySelectorAll('.filter-btn').forEach(function(b){
    b.classList.toggle('active', b.getAttribute('data-filter') === sev);
  });
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::triage::model::{
        DiffFileView, DiffHunkView, DiffLineView, FileChange, Inputs, Summary,
    };
    use atomic_canonical::proof::verify_value;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use std::collections::BTreeMap;

    /// A report fixture whose finding message carries HTML/JS metacharacters,
    /// so escaping can be asserted.
    fn sample_report() -> TriageReport {
        TriageReport {
            reference: "urn:atomic:triage:abc123".to_string(),
            verdict: Verdict::Blocked,
            inputs: Inputs {
                feature: "feature".to_string(),
                target: "dev".to_string(),
                view_merkle: "ZPP4ZK3Z5OKZ".to_string(),
                candidate_changes: vec!["HASH1".to_string()],
                closure_additions: vec![],
                intent_substance_hashes: BTreeMap::new(),
            },
            summary: Summary {
                changes: 1,
                files: 1,
                criteria_met: 0,
                criteria_unmet: 1,
                findings_block: 1,
                findings_warn: 0,
                findings_info: 0,
            },
            intents: vec![IntentReport {
                id: "intent:DEMO-A".to_string(),
                why: Some("Because <it> matters & more".to_string()),
                conforms: false,
                gate_violations: vec!["missing why".to_string()],
                criteria: vec![CriterionReport {
                    id: "urn:atomic:ac:DEMO-A-1".to_string(),
                    text: "a & b < c".to_string(),
                    status: "unmet".to_string(),
                    verified_by: None,
                    judgment_required: true,
                    satisfied_by: vec!["HASH1".to_string()],
                }],
                reviewed_by: Some("did:atomic:<reviewer>&co".to_string()),
            }],
            changes: vec![ChangeReport {
                id: "HASH1".to_string(),
                message: "fix <billing> & taxes".to_string(),
                modifies: vec!["file:src/<a>.rs".to_string()],
                coverage: "uncovered".to_string(),
                files: vec![FileChange {
                    symbol: "~".to_string(),
                    path: "src/<a>.rs".to_string(),
                    summary: "3 hunks & counting".to_string(),
                }],
                diff: vec![DiffFileView {
                    path: "src/<a>.rs".to_string(),
                    status: "modified".to_string(),
                    hunks: vec![DiffHunkView {
                        header: "@@ -1,2 +1,2 @@".to_string(),
                        lines: vec![
                            DiffLineView {
                                tag: "-".to_string(),
                                content: "let x = <old> & 1;".to_string(),
                            },
                            DiffLineView {
                                tag: "+".to_string(),
                                content: "let x = <new> & 2;".to_string(),
                            },
                            DiffLineView {
                                tag: " ".to_string(),
                                content: "return x;".to_string(),
                            },
                        ],
                    }],
                }],
                review_command: "atomic change HASH1 --show-hunks".to_string(),
                blast_radius: vec!["entity:src/util.rs:helper:5".to_string()],
                provenance: None,
            }],
            findings: vec![Finding::new(
                "ORPHAN_CHANGE",
                SEV_BLOCK,
                "HASH1",
                "reaches <script>alert(1)</script> & <b>bold</b>",
            )
            .with_query("atomic vault query neighbors change:HASH1 -d 2 --json")],
        }
    }

    #[test]
    fn render_html_is_structured_and_escaped() {
        let html = render_html(&sample_report());

        // Self-contained: a single document with no external asset references.
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(!html.contains("cdn"));

        // Verdict banner, colored by verdict.
        assert!(html.contains("class=\"banner blocked\""));
        assert!(html.contains("BLOCKED"));

        // Finding code + severity-tagged cards + filter buttons.
        assert!(html.contains("ORPHAN_CHANGE"));
        assert!(html.contains("data-severity=\"block\""));
        assert!(html.contains("onclick=\"filterFindings("));

        // One collapsible <details> per intent.
        assert!(html.contains("<details class=\"intent\""));

        // The reviewer identity is surfaced and HTML-escaped.
        assert!(
            html.contains("class=\"reviewed-by\">reviewed by did:atomic:&lt;reviewer&gt;&amp;co")
        );
        assert!(
            !html.contains("<reviewer>"),
            "reviewer id metacharacters must be escaped"
        );

        // Changes are collapsible too, with a stat summary.
        assert!(html.contains("<details class=\"change\""));
        assert!(html.contains("class=\"stat\""));

        // Per-change review context: message, a file row, and the review command
        // — all HTML-escaped.
        assert!(html.contains("class=\"change-msg\""));
        assert!(html.contains("fix &lt;billing&gt; &amp; taxes"));
        assert!(html.contains("<table class=\"files\">"));
        assert!(html.contains("src/&lt;a&gt;.rs"));
        assert!(html.contains("3 hunks &amp; counting"));
        assert!(html.contains("atomic change HASH1 --show-hunks"));
        assert!(
            !html.contains("<billing>"),
            "raw message metacharacters must be escaped"
        );

        // The real inline diff: a diff block, hunk header, and color-coded
        // add/del lines — all escaped.
        assert!(html.contains("<pre class=\"diff\">"));
        assert!(html.contains("@@ -1,2 +1,2 @@"));
        assert!(html.contains("class=\"line add\">+let x = &lt;new&gt; &amp; 2;"));
        assert!(html.contains("class=\"line del\">-let x = &lt;old&gt; &amp; 1;"));
        assert!(
            !html.contains("<new>") && !html.contains("<old>"),
            "raw diff-line metacharacters must be escaped"
        );

        // Embedded JSON island.
        assert!(html.contains("id=\"triage-report\""));

        // CRITICAL: the raw finding message must be escaped, never injected.
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "unescaped payload must not appear in the output"
        );
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        // The embedded JSON must not contain a raw `<` that could close the tag.
        let json_start = html.find("id=\"triage-report\"").unwrap();
        let json_block = &html[json_start..];
        let script_end = json_block.find("</script>").unwrap();
        assert!(
            !json_block[..script_end].contains('<'),
            "embedded JSON must have all '<' escaped so it cannot break out"
        );
    }

    #[test]
    fn attest_value_roundtrip_verifies() {
        // `attest_report` = resolve_person + this signing path. Test the signing
        // path directly (resolve_person needs a global identity store on disk).
        let report = sample_report();
        let value = serde_json::to_value(&report).unwrap();

        let kp = KeyPair::generate();
        let identity = Identity::new("tester", &kp);
        let signed = attest_value(value, &identity, &kp);

        // The signed artifact is the report Value plus a proof object.
        assert!(signed.get("proof").and_then(|p| p.as_object()).is_some());
        assert_eq!(
            signed.get("reference").and_then(|v| v.as_str()),
            Some("urn:atomic:triage:abc123"),
            "signing must not alter the report's content-addressed reference"
        );

        verify_value(&signed, &identity.public_key).expect("signed report must verify");
    }
}

const HTML_STYLE: &str = r#":root{color-scheme:light}
*{box-sizing:border-box}
body{margin:0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;line-height:1.5;color:#1a1a1a;background:#f6f7f9}
header.banner{padding:24px;color:#fff}
header.banner.blocked{background:#b3261e}
header.banner.stale{background:#b58105}
header.banner.ready{background:#1e7d34}
.verdict{font-size:28px;font-weight:700;letter-spacing:1px}
.route{font-size:18px;margin-top:4px}
.merkle,.counts,.ref{font-size:13px;opacity:.9;margin-top:4px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
section{max-width:960px;margin:20px auto;padding:0 20px}
h2{border-bottom:1px solid #ddd;padding-bottom:6px}
.filters{display:flex;gap:8px;margin:12px 0}
.filter-btn{border:1px solid #ccc;background:#fff;color:#1a1a1a;padding:6px 14px;border-radius:16px;cursor:pointer;font-size:13px}
.filter-btn.active{background:#1a1a1a;color:#fff;border-color:#1a1a1a}
.finding{border-left:4px solid #999;background:#fff;border-radius:6px;padding:12px 14px;margin:10px 0;box-shadow:0 1px 2px rgba(0,0,0,.06)}
.finding.block{border-left-color:#b3261e}
.finding.warn{border-left-color:#b58105}
.finding.info{border-left-color:#1662c4}
.finding-head{display:flex;align-items:center;gap:10px;margin-bottom:4px}
.sev-badge{font-size:11px;text-transform:uppercase;font-weight:700;padding:2px 8px;border-radius:10px;color:#fff}
.sev-badge.block{background:#b3261e}
.sev-badge.warn{background:#b58105}
.sev-badge.info{background:#1662c4}
.code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-weight:600}
.message{margin:2px 0}
.focus,.query,.remedy{font-size:13px;color:#444;margin-top:2px}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#eef0f3;padding:1px 4px;border-radius:4px}
details.intent{background:#fff;border:1px solid #e2e4e8;border-radius:6px;padding:8px 12px;margin:10px 0}
details.intent summary{cursor:pointer;font-weight:600}
details.change summary{cursor:pointer;font-weight:600}
.stat{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;color:#555}
.badge{font-size:11px;padding:2px 8px;border-radius:10px;margin-left:6px}
.badge.ok{background:#1e7d34;color:#fff}
.badge.bad{background:#b3261e;color:#fff}
.badge.judge{background:#b58105;color:#fff}
.badge.cov-covered{background:#1e7d34;color:#fff}
.badge.cov-uncovered{background:#b3261e;color:#fff}
.badge.cov-unknown{background:#777;color:#fff}
.why{color:#333;font-style:italic}
.reviewed-by{color:#1e7d34;font-weight:600;font-size:13px;margin:2px 0}
ul.criteria,ul.violations,ul.modifies{margin:6px 0;padding-left:18px}
.mark.met{color:#1e7d34;font-weight:700}
.mark.unmet{color:#b3261e;font-weight:700}
.change{background:#fff;border:1px solid #e2e4e8;border-radius:6px;padding:10px 12px;margin:10px 0}
.change-msg{margin:4px 0;font-weight:600}
table.files{border-collapse:collapse;margin:6px 0;width:100%}
table.files td{padding:2px 8px;border-bottom:1px solid #f0f1f3;vertical-align:top;font-size:13px}
table.files td.sym{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-weight:700;width:1.5em;text-align:center}
table.files td.summary{color:#555}
.review-cmd{font-size:12px;color:#555;margin-top:6px}
.diff-file{margin-top:10px;font-size:13px}
.diff-path{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-weight:600}
.diff-status{font-size:11px;text-transform:uppercase;padding:1px 6px;border-radius:8px;color:#fff}
.diff-status.added{background:#1e7d34}
.diff-status.deleted{background:#b3261e}
.diff-status.modified{background:#555}
pre.diff{margin:4px 0 0;padding:8px 10px;background:#0d1117;color:#c9d1d9;border-radius:6px;overflow-x:auto;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;line-height:1.45}
pre.diff .hunk-header{color:#8b949e;margin:4px 0 2px}
pre.diff .line{white-space:pre-wrap;word-break:break-word}
pre.diff .line.add{background:rgba(46,160,67,.18);color:#7ee787}
pre.diff .line.del{background:rgba(248,81,73,.18);color:#ffa198}
pre.diff .line.ctx{color:#c9d1d9}
.empty{color:#666;font-style:italic}"#;
