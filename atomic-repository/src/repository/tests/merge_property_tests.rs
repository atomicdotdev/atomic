//! Generative (property) tests for merge and conflict laws.
//!
//! Patch theory is defined by algebraic laws, which makes it a natural target
//! for randomized testing: instead of a hand-picked expectation, each case is
//! checked against a law. These tests drive the real `Repository` API with a
//! seeded, reproducible generator so a failure prints the seed and case.
//!
//! Laws exercised:
//!   * round-trip identity — record → retrieve reproduces the exact bytes
//!   * commutation         — disjoint edits merge to the same bytes in any
//!                           insert order, matching an independent oracle
//!   * idempotence         — re-inserting a present change is a no-op
//!   * honesty             — status ⇔ on-disk markers ⇔ list_conflicts
//!
//! The commutation oracle is a plain line-replacement computed here, NOT a
//! call back into the code under test, so a bug cannot satisfy the property
//! vacuously.

use super::*;
use crate::apply::CrossViewInsertOptions;
use crate::record::{RecordError, RecordOptions};
use crate::status::{FileStatus, StatusOptions};
use atomic_core::change::ChangeHeader;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Fixed default seed so the suite is deterministic. Printed in every failure
/// message; override locally to reproduce a shrunk case.
const SEED: u64 = 0x5EED_A70_C0FFEE;

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    repo.record(
        header,
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
}

/// Generate a single line of content (no embedded newline, never starting
/// with a conflict marker), occasionally empty or containing unicode.
fn gen_line(rng: &mut StdRng) -> String {
    if rng.gen_bool(0.15) {
        return String::new();
    }
    let palette: &[char] = &[
        'a', 'b', 'c', 'd', 'e', 'f', 'g', '0', '1', '2', '3', ' ', '_', '.', 'ü', 'é', '你',
    ];
    let len = rng.gen_range(1..12);
    let mut s = String::new();
    for _ in 0..len {
        s.push(palette[rng.gen_range(0..palette.len())]);
    }
    // Never let a generated line masquerade as a conflict marker.
    if s.starts_with(">>>>>>>") || s.starts_with("=======") || s.starts_with("<<<<<<<") {
        s.insert(0, 'x');
    }
    s
}

fn gen_lines(rng: &mut StdRng, n: usize) -> Vec<String> {
    (0..n).map(|_| gen_line(rng)).collect()
}

/// Join lines with '\n' and a trailing newline (the well-supported shape).
fn assemble(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    if !lines.is_empty() {
        s.push('\n');
    }
    s
}

/// Independent oracle: base with the given (index → new line) replacements.
fn apply_line_edits(base: &[String], edits: &[(usize, String)]) -> Vec<String> {
    let mut out = base.to_vec();
    for (i, line) in edits {
        out[*i] = line.clone();
    }
    out
}

fn disk_bytes(temp: &TempDir, name: &str) -> Vec<u8> {
    std::fs::read(temp.path().join(name)).unwrap_or_default()
}

fn has_markers(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .map(|t| t.lines().any(|l| l.starts_with(">>>>>>>")))
        .unwrap_or(false)
}

// ── Law 1: round-trip identity ──────────────────────────────────────────

#[test]
fn prop_record_roundtrip_is_byte_identical() {
    let mut rng = StdRng::seed_from_u64(SEED);

    // Explicit edge cases first, then randomized bodies.
    let mut cases: Vec<String> = vec![
        String::new(),                        // empty file
        "single line no newline".into(),      // missing trailing newline
        "a\n".into(),                         // single line, trailing newline
        "\n\n\n".into(),                      // only blank lines
        "héllo wörld\nüñîçødé 你好\n".into(), // unicode
    ];
    for _ in 0..16 {
        let n = rng.gen_range(1..8);
        cases.push(assemble(&gen_lines(&mut rng, n)));
    }

    for (i, content) in cases.iter().enumerate() {
        let (_temp, repo) = create_temp_repo();
        let file = _temp.path().join("f.txt");
        std::fs::write(&file, content).unwrap();
        repo.add("f.txt", TrackingOptions::default()).unwrap();
        // An empty new file may legitimately produce "nothing to record" on
        // some configurations; skip only that degenerate case.
        match record_all(&repo, "add") {
            Ok(_) => {}
            Err(RecordError::NothingToRecord) if content.is_empty() => continue,
            Err(e) => panic!("record failed (seed={SEED:#x}, case={i}): {e}"),
        }

        let got = repo
            .get_file_content_on_view("f.txt", repo.current_view())
            .unwrap()
            .unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&got),
            content.as_str(),
            "round-trip mismatch (seed={SEED:#x}, case={i})"
        );
    }
}

// ── Law 2: commutation of disjoint edits ────────────────────────────────

/// Build a fresh repo with `base`, two draft views each replacing one
/// disjoint line, insert them into dev in `order`, materialize, and return
/// the on-disk bytes.
fn build_commuting(
    base: &[String],
    edit_a: (usize, String),
    edit_b: (usize, String),
    order_ab: bool,
) -> Vec<u8> {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");

    std::fs::write(&file, assemble(base)).unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("va", "dev").unwrap();
    repo.switch_view("va").unwrap();
    std::fs::write(&file, assemble(&apply_line_edits(base, &[edit_a.clone()]))).unwrap();
    record_all(&repo, "edit a").unwrap();

    repo.create_view_from("vb", "dev").unwrap();
    repo.switch_view("vb").unwrap();
    std::fs::write(&file, assemble(&apply_line_edits(base, &[edit_b.clone()]))).unwrap();
    record_all(&repo, "edit b").unwrap();

    repo.switch_view("dev").unwrap();
    let (first, second) = if order_ab { ("va", "vb") } else { ("vb", "va") };
    repo.insert_from_view(CrossViewInsertOptions::new(first, "dev"))
        .unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new(second, "dev"))
        .unwrap();
    repo.materialize().unwrap();

    disk_bytes(&temp, "f.txt")
}

/// A disjoint edit for the widened commutation property: boundary
/// insertions as well as interior replacements.
#[derive(Clone, Debug)]
enum Ed {
    /// Insert a new first line.
    Prepend(String),
    /// Insert a new last line.
    Append(String),
    /// Replace an existing line (index into the base).
    Replace(usize, String),
}

fn apply_ed(base: &[String], ed: &Ed) -> Vec<String> {
    let mut v = base.to_vec();
    match ed {
        Ed::Prepend(l) => v.insert(0, l.clone()),
        Ed::Append(l) => v.push(l.clone()),
        Ed::Replace(i, l) => v[*i] = l.clone(),
    }
    v
}

/// Independent oracle for two disjoint edits: replacements first (indices
/// refer to the base), then append, then prepend — order-insensitive for
/// disjoint pairs by construction.
fn apply_both(base: &[String], a: &Ed, b: &Ed) -> Vec<String> {
    let mut v = base.to_vec();
    for ed in [a, b] {
        if let Ed::Replace(i, l) = ed {
            v[*i] = l.clone();
        }
    }
    for ed in [a, b] {
        if let Ed::Append(l) = ed {
            v.push(l.clone());
        }
    }
    for ed in [a, b] {
        if let Ed::Prepend(l) = ed {
            v.insert(0, l.clone());
        }
    }
    v
}

/// Like [`build_commuting`], but for arbitrary disjoint [`Ed`] pairs.
fn build_commuting_eds(base: &[String], a: &Ed, b: &Ed, order_ab: bool) -> Vec<u8> {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");

    std::fs::write(&file, assemble(base)).unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("va", "dev").unwrap();
    repo.switch_view("va").unwrap();
    std::fs::write(&file, assemble(&apply_ed(base, a))).unwrap();
    record_all(&repo, "edit a").unwrap();

    repo.create_view_from("vb", "dev").unwrap();
    repo.switch_view("vb").unwrap();
    std::fs::write(&file, assemble(&apply_ed(base, b))).unwrap();
    record_all(&repo, "edit b").unwrap();

    repo.switch_view("dev").unwrap();
    let (first, second) = if order_ab { ("va", "vb") } else { ("vb", "va") };
    repo.insert_from_view(CrossViewInsertOptions::new(first, "dev"))
        .unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new(second, "dev"))
        .unwrap();
    repo.materialize().unwrap();

    disk_bytes(&temp, "f.txt")
}

/// Commutation including BOUNDARY edits (prepend/append), not just interior
/// replacements.
///
/// History: the generator was originally constrained to interior
/// replacements because the prepend/append pair (harness 27 case 1) was
/// believed to hit a live linearization bug. That defect is fixed in current
/// code (the Aug-7 release binary still reproduces it; harness 27 passes
/// 13/13 against the current binary), so this property now pins the wider
/// space and guards against regressions.
#[test]
fn prop_boundary_and_interior_edits_commute() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 0x4444);

    for case in 0..12 {
        let n = rng.gen_range(3..8);
        let base = gen_lines(&mut rng, n);

        // Disjointness matters at the ANCHOR level, not just the line level.
        // A replacement of line k inserts its new content anchored at the
        // same position a boundary insert on that side uses: Prepend and
        // Replace(0) both anchor right after the inode, Append and
        // Replace(last) both anchor at the file's tail. Those pairs are
        // genuine same-anchor concurrent inserts — a CORRECT conflict (the
        // A4 class), verified by the honesty property, not commutation
        // material. So boundary edits pair with replacements strictly away
        // from their own boundary. (n >= 3 guarantees such an index exists.)
        let new_a = format!("NEWA-{case}");
        let new_b = format!("NEWB-{case}");
        let (a, b) = match case % 4 {
            // The harness-27 case-1 shape: prepend vs append.
            0 => (Ed::Prepend(new_a), Ed::Append(new_b)),
            // Prepend vs replace of a NON-FIRST line.
            1 => (Ed::Prepend(new_a), Ed::Replace(rng.gen_range(1..n), new_b)),
            // Append vs replace of a NON-LAST line.
            2 => (
                Ed::Append(new_a),
                Ed::Replace(rng.gen_range(0..n - 1), new_b),
            ),
            _ => {
                let i = rng.gen_range(0..n);
                let mut j = rng.gen_range(0..n);
                while j == i {
                    j = rng.gen_range(0..n);
                }
                (Ed::Replace(i, new_a), Ed::Replace(j, new_b))
            }
        };

        let ab = build_commuting_eds(&base, &a, &b, true);
        let ba = build_commuting_eds(&base, &a, &b, false);
        let expected = assemble(&apply_both(&base, &a, &b));

        assert!(
            !has_markers(&ab) && !has_markers(&ba),
            "disjoint edits must not conflict (seed={SEED:#x}, case={case}, a={a:?}, b={b:?})\nAB:\n{}\nBA:\n{}",
            String::from_utf8_lossy(&ab),
            String::from_utf8_lossy(&ba),
        );
        assert_eq!(
            ab,
            ba,
            "insert order changed the result (seed={SEED:#x}, case={case}, a={a:?}, b={b:?})\nAB:\n{}\nBA:\n{}",
            String::from_utf8_lossy(&ab),
            String::from_utf8_lossy(&ba),
        );
        assert_eq!(
            String::from_utf8_lossy(&ab),
            expected.as_str(),
            "merge did not match oracle (seed={SEED:#x}, case={case}, a={a:?}, b={b:?})"
        );
    }
}

#[test]
fn prop_disjoint_edits_commute_and_match_oracle() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 0x1111);

    for case in 0..12 {
        let n = rng.gen_range(4..9);
        let base = gen_lines(&mut rng, n);

        // Two distinct interior indices → disjoint replacements.
        let i = rng.gen_range(0..n);
        let mut j = rng.gen_range(0..n);
        while j == i {
            j = rng.gen_range(0..n);
        }
        let line_a = format!("{}-A{}", base[i], case);
        let line_b = format!("{}-B{}", base[j], case);

        let ab = build_commuting(&base, (i, line_a.clone()), (j, line_b.clone()), true);
        let ba = build_commuting(&base, (i, line_a.clone()), (j, line_b.clone()), false);

        // Independent oracle: base with both replacements applied.
        let expected = assemble(&apply_line_edits(
            &base,
            &[(i, line_a.clone()), (j, line_b.clone())],
        ));

        assert!(
            !has_markers(&ab) && !has_markers(&ba),
            "disjoint edits must not conflict (seed={SEED:#x}, case={case})\nAB:\n{}\nBA:\n{}",
            String::from_utf8_lossy(&ab),
            String::from_utf8_lossy(&ba),
        );
        assert_eq!(
            ab,
            ba,
            "insert order changed the result (seed={SEED:#x}, case={case})\nAB:\n{}\nBA:\n{}",
            String::from_utf8_lossy(&ab),
            String::from_utf8_lossy(&ba),
        );
        assert_eq!(
            String::from_utf8_lossy(&ab),
            expected.as_str(),
            "merge did not match oracle (seed={SEED:#x}, case={case})"
        );
    }
}

// ── Law 3: idempotence of insert ────────────────────────────────────────

#[test]
fn prop_reinsert_is_a_noop() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 0x2222);

    for case in 0..10 {
        let n = rng.gen_range(3..8);
        let base = gen_lines(&mut rng, n);
        let i = rng.gen_range(0..n);

        let (temp, mut repo) = create_temp_repo();
        let file = temp.path().join("f.txt");
        std::fs::write(&file, assemble(&base)).unwrap();
        repo.add("f.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "base").unwrap();

        repo.create_view_from("feature", "dev").unwrap();
        repo.switch_view("feature").unwrap();
        let edited = apply_line_edits(&base, &[(i, format!("{}-edit{}", base[i], case))]);
        std::fs::write(&file, assemble(&edited)).unwrap();
        record_all(&repo, "edit").unwrap();

        repo.switch_view("dev").unwrap();
        repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
            .unwrap();
        repo.materialize().unwrap();
        let first = disk_bytes(&temp, "f.txt");

        // Second insert of the same change(s) must apply nothing.
        let again = repo
            .insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
            .unwrap();
        repo.materialize().unwrap();
        let second = disk_bytes(&temp, "f.txt");

        assert_eq!(
            again.changes_applied, 0,
            "re-insert should apply 0 changes (seed={SEED:#x}, case={case})"
        );
        assert_eq!(
            first, second,
            "re-insert changed content / duplicated (seed={SEED:#x}, case={case})"
        );
    }
}

// ── Law 4: conflict honesty ─────────────────────────────────────────────

#[test]
fn prop_conflict_state_is_honest() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 0x3333);

    for case in 0..16 {
        let n = rng.gen_range(4..9);
        let base = gen_lines(&mut rng, n);
        // Half the cases contend on the same line (possible conflict), half
        // edit disjoint lines (clean). Either way the triple must agree.
        let same_line = rng.gen_bool(0.5);
        let i = rng.gen_range(0..n);
        let j = if same_line {
            i
        } else {
            let mut j = rng.gen_range(0..n);
            while j == i {
                j = rng.gen_range(0..n);
            }
            j
        };

        let (temp, mut repo) = create_temp_repo();
        let file = temp.path().join("f.txt");
        std::fs::write(&file, assemble(&base)).unwrap();
        repo.add("f.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "base").unwrap();

        repo.create_view_from("feature", "dev").unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(
            &file,
            assemble(&apply_line_edits(&base, &[(i, format!("AAA-{case}"))])),
        )
        .unwrap();
        record_all(&repo, "edit a").unwrap();

        repo.switch_view("dev").unwrap();
        std::fs::write(
            &file,
            assemble(&apply_line_edits(&base, &[(j, format!("BBB-{case}"))])),
        )
        .unwrap();
        record_all(&repo, "edit b").unwrap();

        repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
            .unwrap();
        repo.materialize().unwrap();

        let markers = has_markers(&disk_bytes(&temp, "f.txt"));

        let status = repo.status(StatusOptions::default()).unwrap();
        let status_conf = status
            .entries()
            .iter()
            .any(|e| e.path().to_string_lossy() == "f.txt" && e.status() == FileStatus::Conflicted);

        let listed = repo
            .list_conflicts()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "f.txt");

        assert_eq!(
            markers, status_conf,
            "honesty broken: markers={markers} but status_conflicted={status_conf} \
             (seed={SEED:#x}, case={case}, same_line={same_line})"
        );
        assert_eq!(
            status_conf, listed,
            "honesty broken: status_conflicted={status_conf} but list_conflicts={listed} \
             (seed={SEED:#x}, case={case}, same_line={same_line})"
        );
    }
}
