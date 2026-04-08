//! Transfer progress reporting and statistics.
//!
//! Contains [`TransferProgress`] for per-section progress events emitted
//! during streaming transfers, and [`TransferStats`] for summary statistics
//! after a transfer completes.

use std::fmt;

use super::types::format_size;

// ═══════════════════════════════════════════════════════════════════════
// TransferProgress — per-section progress reporting
// ═══════════════════════════════════════════════════════════════════════

/// Progress event emitted during a streaming transfer.
///
/// These events are emitted as sections are transferred, enabling
/// real-time progress reporting in the CLI.
///
/// # Event Flow (Push)
///
/// ```text
/// Started { total_sections: 11, total_bytes: 7500000 }
/// SectionComplete { section: "HEADER", bytes: 200 }
/// SectionComplete { section: "DEPS", bytes: 50 }
/// SectionComplete { section: "GRAPH #1", bytes: 15000 }
/// ChunkComplete { index: 0, bytes: 32000, hash: "aabb..." }
/// ChunkComplete { index: 1, bytes: 28000, hash: "ccdd..." }
/// ...
/// Finished { total_bytes: 7500000, elapsed_ms: 1200 }
/// ```
#[derive(Clone, Debug)]
pub enum TransferProgress {
    /// Transfer is starting.
    Started {
        /// Total number of sections to transfer.
        total_sections: u32,
        /// Estimated total bytes to transfer (compressed).
        total_bytes_estimate: u64,
    },

    /// A metadata or layer section was transferred.
    SectionComplete {
        /// Human-readable section description (e.g., "HEADER", "GRAPH #3").
        section: String,
        /// Compressed bytes transferred for this section.
        bytes_transferred: u64,
    },

    /// A content chunk was transferred.
    ChunkComplete {
        /// Chunk index.
        index: u32,
        /// Compressed bytes transferred.
        bytes_transferred: u32,
        /// Was this chunk skipped (already present on receiver)?
        skipped: bool,
    },

    /// Transfer is complete.
    Finished {
        /// Total compressed bytes transferred.
        total_bytes: u64,
        /// Wall-clock elapsed time in milliseconds.
        elapsed_ms: u64,
    },
}

impl fmt::Display for TransferProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransferProgress::Started {
                total_sections,
                total_bytes_estimate,
            } => write!(
                f,
                "Starting transfer: {} sections, ~{}",
                total_sections,
                format_size(*total_bytes_estimate),
            ),
            TransferProgress::SectionComplete {
                section,
                bytes_transferred,
            } => write!(f, "  {} {}", section, format_size(*bytes_transferred),),
            TransferProgress::ChunkComplete {
                index,
                bytes_transferred,
                skipped,
            } => {
                if *skipped {
                    write!(f, "  chunk #{} (skipped — already present)", index)
                } else {
                    write!(
                        f,
                        "  chunk #{} {}",
                        index,
                        format_size(*bytes_transferred as u64),
                    )
                }
            }
            TransferProgress::Finished {
                total_bytes,
                elapsed_ms,
            } => write!(
                f,
                "Transfer complete: {} in {:.1}s",
                format_size(*total_bytes),
                *elapsed_ms as f64 / 1000.0,
            ),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// TransferStats — summary statistics for a completed transfer
// ═══════════════════════════════════════════════════════════════════════

/// Summary statistics for a completed streaming transfer.
///
/// Available after a push or pull operation completes. Useful for
/// logging, performance analysis, and displaying summaries to the user.
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::TransferStats;
///
/// let stats = TransferStats {
///     sections_transferred: 11,
///     chunks_transferred: 5,
///     chunks_skipped: 3,
///     bytes_transferred: 75000,
///     bytes_skipped: 60000,
///     elapsed_ms: 1200,
/// };
///
/// assert_eq!(stats.total_chunks(), 8);
/// assert!((stats.savings_pct() - 44.4).abs() < 1.0);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// Number of sections transferred (including metadata, graph, semantic).
    pub sections_transferred: u32,

    /// Number of content chunks actually transferred (not skipped).
    pub chunks_transferred: u32,

    /// Number of content chunks skipped (already present on receiver).
    pub chunks_skipped: u32,

    /// Total compressed bytes actually transferred.
    pub bytes_transferred: u64,

    /// Total compressed bytes saved by delta transfer (skipped chunks).
    pub bytes_skipped: u64,

    /// Wall-clock elapsed time in milliseconds.
    pub elapsed_ms: u64,
}

impl TransferStats {
    /// Total chunks (transferred + skipped).
    pub fn total_chunks(&self) -> u32 {
        self.chunks_transferred + self.chunks_skipped
    }

    /// Total bytes (transferred + skipped).
    pub fn total_bytes(&self) -> u64 {
        self.bytes_transferred + self.bytes_skipped
    }

    /// Percentage of bytes saved by delta transfer (0.0 to 100.0).
    pub fn savings_pct(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            return 0.0;
        }
        self.bytes_skipped as f64 / total as f64 * 100.0
    }

    /// Effective transfer rate in bytes per second.
    ///
    /// Returns 0 if elapsed_ms is 0.
    pub fn bytes_per_second(&self) -> u64 {
        if self.elapsed_ms == 0 {
            return 0;
        }
        self.bytes_transferred * 1000 / self.elapsed_ms
    }
}

impl fmt::Display for TransferStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} sections, {} chunks transferred",
            self.sections_transferred, self.chunks_transferred,
        )?;

        if self.chunks_skipped > 0 {
            write!(
                f,
                " ({} skipped, {:.1}% savings)",
                self.chunks_skipped,
                self.savings_pct(),
            )?;
        }

        write!(
            f,
            ", {} in {:.1}s",
            format_size(self.bytes_transferred),
            self.elapsed_ms as f64 / 1000.0,
        )?;

        let bps = self.bytes_per_second();
        if bps > 0 {
            write!(f, " ({}/s)", format_size(bps))?;
        }

        Ok(())
    }
}
