//! Output formatting methods for the diff command.
//!
//! This module contains the display/printing logic extracted from the
//! `Diff` command: unified diff, stat summary, name-only, and name-status.

use super::output::*;
use super::*;

impl Diff {
    /// Print the diff in unified format.
    pub(super) fn print_unified(
        &self,
        file_diffs: &[FileDiff],
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        for file_diff in file_diffs {
            // Print file header
            let old_path = if config.show_path_prefix {
                format!("a/{}", file_diff.old_path)
            } else {
                file_diff.old_path.clone()
            };
            let new_path = if config.show_path_prefix {
                format!("b/{}", file_diff.new_path)
            } else {
                file_diff.new_path.clone()
            };

            // Format line stats (e.g., "+2 -1") with colors
            let line_stats = if file_diff.stats.insertions > 0 || file_diff.stats.deletions > 0 {
                let ins = if file_diff.stats.insertions > 0 {
                    format!("+{}", file_diff.stats.insertions)
                } else {
                    String::new()
                };
                let del = if file_diff.stats.deletions > 0 {
                    format!("-{}", file_diff.stats.deletions)
                } else {
                    String::new()
                };
                (ins, del)
            } else {
                (String::new(), String::new())
            };

            if config.color {
                // Build colored line stats
                let colored_stats = if !line_stats.0.is_empty() || !line_stats.1.is_empty() {
                    let ins_colored = if !line_stats.0.is_empty() {
                        added(&line_stats.0).to_string()
                    } else {
                        String::new()
                    };
                    let del_colored = if !line_stats.1.is_empty() {
                        deleted(&line_stats.1).to_string()
                    } else {
                        String::new()
                    };
                    if !ins_colored.is_empty() && !del_colored.is_empty() {
                        format!(" ({} {})", ins_colored, del_colored)
                    } else if !ins_colored.is_empty() {
                        format!(" ({})", ins_colored)
                    } else {
                        format!(" ({})", del_colored)
                    }
                } else {
                    String::new()
                };
                println!(
                    "{}{}",
                    emphasis(&format!("diff --atomic {} {}", old_path, new_path)),
                    colored_stats
                );
                println!("{}", deleted(&format!("--- {}", old_path)));
                println!("{}", added(&format!("+++ {}", new_path)));
            } else {
                // Non-colored output
                let plain_stats = if !line_stats.0.is_empty() || !line_stats.1.is_empty() {
                    if !line_stats.0.is_empty() && !line_stats.1.is_empty() {
                        format!(" ({} {})", line_stats.0, line_stats.1)
                    } else if !line_stats.0.is_empty() {
                        format!(" ({})", line_stats.0)
                    } else {
                        format!(" ({})", line_stats.1)
                    }
                } else {
                    String::new()
                };
                println!("diff --atomic {} {}{}", old_path, new_path, plain_stats);
                println!("--- {}", old_path);
                println!("+++ {}", new_path);
            }

            // Handle binary files
            if file_diff.is_binary {
                println!("Binary files differ");
                continue;
            }

            // Print each hunk
            for graph_op in &file_diff.hunks {
                // Print hunk header
                if config.color {
                    println!("{}", info(&graph_op.header()));
                } else {
                    println!("{}", graph_op.header());
                }

                // Print hunk lines with optional word-level highlighting
                // Collect consecutive removed and added lines to pair them correctly
                let mut i = 0;
                while i < graph_op.lines.len() {
                    let line = &graph_op.lines[i];

                    // Check if we can do word-level diff
                    if config.color && line.status == LineStatus::Removed {
                        // Collect all consecutive removed lines
                        let mut removed_lines: Vec<&HunkLine> = vec![line];
                        let mut j = i + 1;
                        while j < graph_op.lines.len()
                            && graph_op.lines[j].status == LineStatus::Removed
                        {
                            removed_lines.push(&graph_op.lines[j]);
                            j += 1;
                        }

                        // Collect all consecutive added lines that follow
                        let mut added_lines: Vec<&HunkLine> = Vec::new();
                        while j < graph_op.lines.len()
                            && graph_op.lines[j].status == LineStatus::Added
                        {
                            added_lines.push(&graph_op.lines[j]);
                            j += 1;
                        }

                        // If we have both removed and added lines, pair them for word-level diff
                        if !added_lines.is_empty() {
                            let pairs = removed_lines.len().min(added_lines.len());

                            // Process paired lines with word-level highlighting
                            for k in 0..pairs {
                                let removed_line = removed_lines[k];
                                let added_line = added_lines[k];

                                // Use semantic diff for better token-level highlighting
                                let old_content = removed_line.content.as_bytes();
                                let new_content = added_line.content.as_bytes();

                                // Compute semantic diff for precise token boundaries
                                let sem_diff = semantic_diff(old_content, new_content);

                                let mut used_semantic = false;
                                #[allow(clippy::collapsible_match)]
                                if let Some(change) = sem_diff.changes().first() {
                                    if let LineChange::Modified { token_changes, .. } = change {
                                        // Print old line with semantic token highlighting
                                        let old_num_str = if config.show_line_numbers {
                                            format!(
                                                "{:>4} {:>4} ",
                                                removed_line
                                                    .old_line_num
                                                    .map(|n| n.to_string())
                                                    .unwrap_or_default(),
                                                ""
                                            )
                                        } else {
                                            String::new()
                                        };
                                        print!(
                                            "{}",
                                            deleted(&format!(
                                                "{}{}",
                                                old_num_str,
                                                removed_line.prefix()
                                            ))
                                        );
                                        print_semantic_word_diff_line(token_changes, true);
                                        println!();

                                        // Print new line with semantic token highlighting
                                        let new_num_str = if config.show_line_numbers {
                                            format!(
                                                "{:>4} {:>4} ",
                                                "",
                                                added_line
                                                    .new_line_num
                                                    .map(|n| n.to_string())
                                                    .unwrap_or_default()
                                            )
                                        } else {
                                            String::new()
                                        };
                                        print!(
                                            "{}",
                                            added(&format!(
                                                "{}{}",
                                                new_num_str,
                                                added_line.prefix()
                                            ))
                                        );
                                        print_semantic_word_diff_line(token_changes, false);
                                        println!();

                                        used_semantic = true;
                                    }
                                }

                                if !used_semantic {
                                    // Fallback to inline diff if semantic diff didn't work
                                    let inline_diff = compute_inline_diff(old_content, new_content);

                                    // Print old line with word-level highlighting
                                    let old_num_str = if config.show_line_numbers {
                                        format!(
                                            "{:>4} {:>4} ",
                                            removed_line
                                                .old_line_num
                                                .map(|n| n.to_string())
                                                .unwrap_or_default(),
                                            ""
                                        )
                                    } else {
                                        String::new()
                                    };
                                    print!(
                                        "{}",
                                        deleted(&format!(
                                            "{}{}",
                                            old_num_str,
                                            removed_line.prefix()
                                        ))
                                    );
                                    print_word_diff_line(
                                        old_content,
                                        inline_diff.old_hunks(),
                                        true,
                                    );
                                    println!();

                                    // Print new line with word-level highlighting
                                    let new_num_str = if config.show_line_numbers {
                                        format!(
                                            "{:>4} {:>4} ",
                                            "",
                                            added_line
                                                .new_line_num
                                                .map(|n| n.to_string())
                                                .unwrap_or_default()
                                        )
                                    } else {
                                        String::new()
                                    };
                                    print!(
                                        "{}",
                                        added(&format!("{}{}", new_num_str, added_line.prefix()))
                                    );
                                    print_word_diff_line(
                                        new_content,
                                        inline_diff.new_hunks(),
                                        false,
                                    );
                                    println!();
                                }
                            }

                            // Print any remaining unpaired removed lines
                            for removed_line in removed_lines.iter().skip(pairs) {
                                let removed_line = *removed_line;
                                let line_num_str = if config.show_line_numbers {
                                    format!(
                                        "{:>4} {:>4} ",
                                        removed_line
                                            .old_line_num
                                            .map(|n| n.to_string())
                                            .unwrap_or_default(),
                                        ""
                                    )
                                } else {
                                    String::new()
                                };
                                let formatted = format!(
                                    "{}{}{}",
                                    line_num_str,
                                    removed_line.prefix(),
                                    removed_line.content
                                );
                                println!("{}", deleted(&formatted));
                            }

                            // Print any remaining unpaired added lines
                            for added_line in added_lines.iter().skip(pairs) {
                                let added_line = *added_line;
                                let line_num_str = if config.show_line_numbers {
                                    format!(
                                        "{:>4} {:>4} ",
                                        "",
                                        added_line
                                            .new_line_num
                                            .map(|n| n.to_string())
                                            .unwrap_or_default()
                                    )
                                } else {
                                    String::new()
                                };
                                let formatted = format!(
                                    "{}{}{}",
                                    line_num_str,
                                    added_line.prefix(),
                                    added_line.content
                                );
                                println!("{}", added(&formatted));
                            }

                            // Skip all processed lines
                            i = j;
                            continue;
                        }
                    }

                    // Standard line output (no word-level diff)
                    let line_num_str = if config.show_line_numbers {
                        match line.status {
                            LineStatus::Added => {
                                format!(
                                    "{:>4} {:>4} ",
                                    "",
                                    line.new_line_num.map(|n| n.to_string()).unwrap_or_default()
                                )
                            }
                            LineStatus::Removed => {
                                format!(
                                    "{:>4} {:>4} ",
                                    line.old_line_num.map(|n| n.to_string()).unwrap_or_default(),
                                    ""
                                )
                            }
                            LineStatus::Unchanged => {
                                format!(
                                    "{:>4} {:>4} ",
                                    line.old_line_num.map(|n| n.to_string()).unwrap_or_default(),
                                    line.new_line_num.map(|n| n.to_string()).unwrap_or_default()
                                )
                            }
                        }
                    } else {
                        String::new()
                    };
                    let formatted = format!("{}{}{}", line_num_str, line.prefix(), line.content);
                    if config.color {
                        match line.status {
                            LineStatus::Added => println!("{}", added(&formatted)),
                            LineStatus::Removed => println!("{}", deleted(&formatted)),
                            LineStatus::Unchanged => println!("{}", formatted),
                        }
                    } else {
                        println!("{}", formatted);
                    }
                    i += 1;
                }
            }
        }

        Ok(())
    }

    /// Print the diff in stat format.
    pub(super) fn print_stat(&self, stats: &DiffStats, config: &DiffOutputConfig) -> CliResult<()> {
        if !stats.has_changes() {
            return Ok(());
        }

        let max_path_len = stats.max_path_length();
        let max_changes = stats.max_change_count();
        let graph_width = cmp::min(config.stat_width, max_changes);

        for file_stats in stats.iter() {
            let path = &file_stats.path;
            let padding = max_path_len - path.len();
            let total = file_stats.total_changes();

            // Calculate graph
            let graph = if total > 0 && graph_width > 0 {
                let scale = if max_changes > graph_width {
                    graph_width as f64 / max_changes as f64
                } else {
                    1.0
                };
                let plus_count = ((file_stats.insertions as f64 * scale).round() as usize)
                    .max(if file_stats.insertions > 0 { 1 } else { 0 });
                let minus_count = ((file_stats.deletions as f64 * scale).round() as usize)
                    .max(if file_stats.deletions > 0 { 1 } else { 0 });
                format!("{}{}", "+".repeat(plus_count), "-".repeat(minus_count))
            } else {
                String::new()
            };

            if config.color {
                let plus_part = "+".repeat(file_stats.insertions.min(graph_width));
                let minus_part = "-".repeat(file_stats.deletions.min(graph_width));
                println!(
                    " {} {} | {} {}{}",
                    style_path(path),
                    " ".repeat(padding),
                    total,
                    added(&plus_part),
                    deleted(&minus_part)
                );
            } else {
                println!(" {} {} | {} {}", path, " ".repeat(padding), total, graph);
            }
        }

        // Print summary
        let files_text = if stats.file_count() == 1 {
            "file"
        } else {
            "files"
        };
        let ins_text = if stats.total_insertions() == 1 {
            "insertion"
        } else {
            "insertions"
        };
        let del_text = if stats.total_deletions() == 1 {
            "deletion"
        } else {
            "deletions"
        };

        println!(
            " {} {} changed, {} {}(+), {} {}(-)",
            stats.file_count(),
            files_text,
            stats.total_insertions(),
            ins_text,
            stats.total_deletions(),
            del_text
        );

        Ok(())
    }

    /// Print file names only.
    pub(super) fn print_name_only(&self, file_diffs: &[FileDiff]) -> CliResult<()> {
        for file_diff in file_diffs {
            println!("{}", file_diff.display_path());
        }
        Ok(())
    }

    /// Print file names with status.
    pub(super) fn print_name_status(
        &self,
        file_diffs: &[FileDiff],
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        for file_diff in file_diffs {
            let status_char = file_diff.status.status_char();
            let path = file_diff.display_path();

            if config.color {
                let status_str = status_char.to_string();
                let styled_status = match file_diff.status {
                    FileChangeStatus::Added => added(&status_str),
                    FileChangeStatus::Deleted => deleted(&status_str),
                    FileChangeStatus::Modified => modified(&status_str),
                    _ => info(&status_str),
                };
                println!("{}  {}", styled_status, style_path(path));
            } else {
                println!("{}  {}", status_char, path);
            }
        }
        Ok(())
    }
}
