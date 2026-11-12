//! Lower first-parent Git line-index updates into immutable span operations.
//!
//! The graph compiler supplies namespace hunks and content offsets. This
//! index tracks which authored span owns each currently visible line.

use super::*;
use atomic_core::change::span_patch::{ExternalFile, ExternalSpan, SpanOp};

#[derive(Clone)]
struct SpanLine {
    span: ExternalSpan,
    start: u64,
    end: u64,
}

#[derive(Clone)]
struct SpanFile {
    file: ExternalFile,
    root: ExternalSpan,
    lines: Vec<SpanLine>,
}

#[derive(Default)]
pub(super) struct SpanCompiler {
    files: HashMap<String, SpanFile>,
}

pub(super) struct NewRoot {
    path: String,
    ordinal: u32,
    start: u64,
    end: u64,
}

impl SpanCompiler {
    pub(super) fn lower(
        &self,
        pending: &[PendingLineIndexUpdate],
        hunks: &mut Vec<GraphOp<Option<ContentHash>>>,
    ) -> CliResult<(Vec<SpanOp>, Vec<NewRoot>)> {
        let mut ops = Vec::new();
        let mut roots = Vec::new();
        for update in pending {
            match update {
                PendingLineIndexUpdate::Add {
                    path, new_ranges, ..
                } => {
                    let (empty_start, root_path) = hunks
                        .iter()
                        .find_map(|hunk| {
                            if let GraphOp::FileAdd {
                                path: name,
                                add_inode,
                                ..
                            } = hunk
                            {
                                (atomic_repository::canonical_path_identity(name)
                                    == atomic_repository::canonical_path_identity(path))
                                .then_some((add_inode.start.get(), name.clone()))
                            } else {
                                None
                            }
                        })
                        .ok_or_else(|| {
                            git_error(format!("span add has no namespace operation for {path}"))
                        })?;
                    let start = new_ranges
                        .first()
                        .map_or(empty_start, |range| range.0.get());
                    let end = new_ranges.last().map_or(start, |range| range.1.get());
                    let ordinal = u32::try_from(roots.len())
                        .map_err(|_| git_error("too many span roots in one change"))?;
                    ops.push(SpanOp::Root {
                        path: root_path,
                        ordinal,
                        start,
                        end,
                    });
                    roots.push(NewRoot {
                        path: path.clone(),
                        ordinal,
                        start,
                        end,
                    });
                }
                PendingLineIndexUpdate::Modify { path, replacements } => {
                    let file = self.files.get(path).ok_or_else(|| {
                        git_error(format!("span file identity missing for {path}"))
                    })?;
                    for replacement in replacements {
                        let begin = replacement.start_idx;
                        let end = begin
                            .checked_add(replacement.old_len)
                            .ok_or_else(|| git_error("span replacement overflow"))?;
                        if end > file.lines.len() {
                            return Err(git_error("span replacement exceeds current file"));
                        }
                        let insert_start = replacement
                            .new_ranges
                            .first()
                            .map_or(0, |range| range.0.get());
                        let insert_end = replacement
                            .new_ranges
                            .last()
                            .map_or(insert_start, |range| range.1.get());
                        if begin == end {
                            let (target, at) = if begin > 0 {
                                let line = &file.lines[begin - 1];
                                (line.span.clone(), line.end)
                            } else if let Some(line) = file.lines.first() {
                                (line.span.clone(), line.start)
                            } else {
                                (file.root.clone(), file.root.start)
                            };
                            ops.push(SpanOp::Edit {
                                file: file.file.clone(),
                                start: at - target.start,
                                end: at - target.start,
                                target,
                                insert_start,
                                insert_end,
                            });
                            continue;
                        }
                        let mut current = begin;
                        let mut first = true;
                        while current < end {
                            let line = &file.lines[current];
                            let target = line.span.clone();
                            let start = line.start;
                            let mut finish = line.end;
                            current += 1;
                            while current < end {
                                let next = &file.lines[current];
                                if next.span != target || next.start != finish {
                                    break;
                                }
                                finish = next.end;
                                current += 1;
                            }
                            ops.push(SpanOp::Edit {
                                file: file.file.clone(),
                                target: target.clone(),
                                start: start - target.start,
                                end: finish - target.start,
                                insert_start: if first { insert_start } else { 0 },
                                insert_end: if first { insert_end } else { 0 },
                            });
                            first = false;
                        }
                    }
                }
                PendingLineIndexUpdate::Rename { .. } => {
                    return Err(git_error(
                        "span compiler requires normalized delete/add renames",
                    ))
                }
                PendingLineIndexUpdate::DirectoryAdd { .. }
                | PendingLineIndexUpdate::Delete { .. } => {}
            }
        }
        // Preserve namespace and attributes, but do not duplicate content as
        // per-line vertices or adjacency edges in the ordinary graph tables.
        hunks.retain_mut(|hunk| match hunk {
            GraphOp::FileAdd { contents, .. } => {
                *contents = None;
                true
            }
            GraphOp::FileDel { del, contents, .. } => {
                del.edges.clear();
                *contents = None;
                true
            }
            GraphOp::DirAdd { .. } | GraphOp::DirDel { .. } | GraphOp::SetAttr { .. } => true,
            GraphOp::Edit { .. } | GraphOp::Replacement { .. } => false,
            _ => true,
        });
        Ok((ops, roots))
    }

    pub(super) fn finish(
        &mut self,
        hash: ContentHash,
        pending: &[PendingLineIndexUpdate],
        roots: &[NewRoot],
    ) -> CliResult<()> {
        for update in pending {
            match update {
                PendingLineIndexUpdate::Add {
                    path, new_ranges, ..
                } => {
                    let root = roots
                        .iter()
                        .find(|root| &root.path == path)
                        .ok_or_else(|| git_error("span root missing after compilation"))?;
                    let span = ExternalSpan {
                        owner: hash,
                        start: root.start,
                        end: root.end,
                    };
                    let lines = new_ranges
                        .iter()
                        .map(|(start, end)| SpanLine {
                            span: span.clone(),
                            start: start.get(),
                            end: end.get(),
                        })
                        .collect();
                    self.files.insert(
                        path.clone(),
                        SpanFile {
                            file: ExternalFile {
                                owner: hash,
                                ordinal: root.ordinal,
                            },
                            root: span,
                            lines,
                        },
                    );
                }
                PendingLineIndexUpdate::Modify { path, replacements } => {
                    let file = self
                        .files
                        .get_mut(path)
                        .ok_or_else(|| git_error("span file vanished"))?;
                    for replacement in replacements.iter().rev() {
                        let start = replacement.start_idx;
                        let end = start + replacement.old_len;
                        let lines = if let (Some(first), Some(last)) = (
                            replacement.new_ranges.first(),
                            replacement.new_ranges.last(),
                        ) {
                            let span = ExternalSpan {
                                owner: hash,
                                start: first.0.get(),
                                end: last.1.get(),
                            };
                            replacement
                                .new_ranges
                                .iter()
                                .map(|(lo, hi)| SpanLine {
                                    span: span.clone(),
                                    start: lo.get(),
                                    end: hi.get(),
                                })
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                        file.lines.splice(start..end, lines);
                    }
                }
                PendingLineIndexUpdate::Delete { path } => {
                    self.files.remove(path);
                }
                PendingLineIndexUpdate::DirectoryAdd { .. } => {}
                PendingLineIndexUpdate::Rename { .. } => {
                    return Err(git_error(
                        "span compiler cannot preserve Git rename identity",
                    ))
                }
            }
        }
        Ok(())
    }
}
