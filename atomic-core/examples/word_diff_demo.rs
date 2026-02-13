//! Word-Level Diff Demo for Code Reviews
//!
//! This example demonstrates the CRDT-style word-level differencing capability
//! that enables GitHub/GitLab-style code review highlighting:
//!
//! - Light background: Shows that a line changed
//! - Dark highlight: Shows exactly which words/tokens changed within the line
//!
//! Run with: cargo run --example word_diff_demo

use atomic_core::diff::{
    compute_inline_diff, Algorithm, DiffOp, Line, HunkKind, Tokenizer,
};

fn main() {
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║          Word-Level Diff Demo for Code Reviews                   ║");
    println!("╚══════════════════════════════════════════════════════════════════╝\n");

    // Example 1: Function argument added
    demo_inline_diff(
        "Example 1: Function Argument Added",
        b"const result = calculateSum(a, b);",
        b"const result = calculateSum(a, b, c);",
    );

    // Example 2: Variable value changed
    demo_inline_diff(
        "Example 2: Variable Value Changed",
        b"let timeout = 5000;",
        b"let timeout = 10000;",
    );

    // Example 3: Operator changed
    demo_inline_diff(
        "Example 3: Operator Changed",
        b"if (x == y) {",
        b"if (x != y) {",
    );

    // Example 4: Variable renamed
    demo_inline_diff(
        "Example 4: Variable Renamed",
        b"let foo = getValue();",
        b"let bar = getValue();",
    );

    // Example 5: Type annotation added
    demo_inline_diff(
        "Example 5: Type Annotation Added",
        b"let count = 0",
        b"let count: i32 = 0",
    );

    // Example 6: String literal changed
    demo_inline_diff(
        "Example 6: String Literal Changed",
        b"let message = \"Hello, World!\";",
        b"let message = \"Goodbye, World!\";",
    );

    // Multi-line diff example
    println!("\n======================================================================\n");
    demo_multiline_diff();

    // Tokenizer demo
    println!("\n======================================================================\n");
    demo_tokenizer();
}

/// Demonstrate inline diff between two lines
fn demo_inline_diff(title: &str, old: &[u8], new: &[u8]) {
    println!("┌─ {} ─┐", title);
    println!("│");

    let diff = compute_inline_diff(old, new);

    // Render old line with deletions highlighted
    print!("│ \x1b[101m - \x1b[0m "); // Light red background for line prefix
    print_highlighted_line(old, diff.old_hunks(), true);
    println!();

    // Render new line with insertions highlighted
    print!("│ \x1b[102m + \x1b[0m "); // Light green background for line prefix
    print_highlighted_line(new, diff.new_hunks(), false);
    println!();

    println!("│");
    println!(
        "│ Stats: {} bytes deleted, {} bytes inserted",
        diff.deleted_bytes(),
        diff.inserted_bytes()
    );
    println!("└────────────────────────────────────────────────────────────────────┘\n");
}

/// Print a line with highlighted hunks
fn print_highlighted_line(
    content: &[u8],
    hunks: &[atomic_core::diff::ChangeHunk],
    is_deletion: bool,
) {
    for hunk in hunks {
        let text = String::from_utf8_lossy(&content[hunk.start..hunk.end]);

        match hunk.kind {
            HunkKind::Deleted | HunkKind::Modified if is_deletion => {
                // Dark red for deleted content
                print!("\x1b[91;1m{}\x1b[0m", text);
            }
            HunkKind::Inserted | HunkKind::Modified if !is_deletion => {
                // Dark green for inserted content
                print!("\x1b[92;1m{}\x1b[0m", text);
            }
            _ => {
                // Normal text (unchanged)
                print!("{}", text);
            }
        }
    }
}

/// Demonstrate multi-line diff with word-level highlighting
fn demo_multiline_diff() {
    println!("┌─ Multi-Line Diff Example ─┐");
    println!("│");

    let old_code = b"pub fn process(items: Vec<Item>) -> Result<(), Error> {
    for item in items {
        validate(item)?;
    }
    Ok(())
}";

    let new_code = b"pub fn process(items: &[Item]) -> Result<(), ProcessError> {
    for item in items.iter() {
        validate_item(item)?;
    }
    Ok(())
}";

    // Split into lines and diff
    let old_lines: Vec<Line> = Line::from_bytes(old_code);
    let new_lines: Vec<Line> = Line::from_bytes(new_code);

    let line_diff = atomic_core::diff::diff(&old_lines, &new_lines, Algorithm::Myers);

    let mut old_idx = 0;
    let mut new_idx = 0;

    for op in line_diff.iter() {
        match op {
            DiffOp::Equal { len, .. } => {
                // Show equal lines without highlighting
                for _ in 0..*len {
                    let line_content = old_lines[old_idx].content();
                    let text = String::from_utf8_lossy(line_content);
                    println!("│   {}", text.trim_end());
                    old_idx += 1;
                    new_idx += 1;
                }
            }
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                // For replaced lines, show word-level diff
                for i in 0..*old_len.max(new_len) {
                    let old_line = if i < *old_len {
                        Some(old_lines[old_idx + i].content())
                    } else {
                        None
                    };
                    let new_line = if i < *new_len {
                        Some(new_lines[new_idx + i].content())
                    } else {
                        None
                    };

                    match (old_line, new_line) {
                        (Some(old), Some(new)) => {
                            // Both lines exist - show word diff
                            let inline = compute_inline_diff(old, new);

                            print!("│ \x1b[101m-\x1b[0m ");
                            print_highlighted_line(old, inline.old_hunks(), true);
                            println!();

                            print!("│ \x1b[102m+\x1b[0m ");
                            print_highlighted_line(new, inline.new_hunks(), false);
                            println!();
                        }
                        (Some(old), None) => {
                            // Only old line - pure deletion
                            let text = String::from_utf8_lossy(old);
                            println!("│ \x1b[91m- {}\x1b[0m", text.trim_end());
                        }
                        (None, Some(new)) => {
                            // Only new line - pure insertion
                            let text = String::from_utf8_lossy(new);
                            println!("│ \x1b[92m+ {}\x1b[0m", text.trim_end());
                        }
                        (None, None) => {}
                    }
                }
                old_idx += old_len;
                new_idx += new_len;
            }
            DiffOp::Delete { len, .. } => {
                for _ in 0..*len {
                    let text = String::from_utf8_lossy(old_lines[old_idx].content());
                    println!("│ \x1b[91m- {}\x1b[0m", text.trim_end());
                    old_idx += 1;
                }
            }
            DiffOp::Insert { len, .. } => {
                for _ in 0..*len {
                    let text = String::from_utf8_lossy(new_lines[new_idx].content());
                    println!("│ \x1b[92m+ {}\x1b[0m", text.trim_end());
                    new_idx += 1;
                }
            }
        }
    }

    println!("│");
    println!("└────────────────────────────────────────────────────────────────────┘");
}

/// Demonstrate the tokenizer
fn demo_tokenizer() {
    println!("┌─ Tokenizer Demo ─┐");
    println!("│");

    let code = b"let result: i32 = calculate(x + y) * 2; // compute";
    println!(
        "│ Input: {}",
        String::from_utf8_lossy(code)
    );
    println!("│");
    println!("│ Tokens:");

    for token in Tokenizer::new(code) {
        println!(
            "│   {:12} │ {:15} │ @{:2}..{:2}",
            format!("{:?}", token.kind()),
            format!("\"{}\"", token.as_str().escape_default()),
            token.offset(),
            token.end_offset()
        );
    }

    println!("│");
    println!("└────────────────────────────────────────────────────────────────────┘");

    println!("\n✓ Word-level diff enables precise code review highlighting!");
    println!("  Use light backgrounds for changed lines, dark highlights for changed tokens.");
}
