//! Vault embedding pipeline — chunking, storage, and search.
//!
//! The embedding pipeline is pluggable: the actual vector computation
//! can come from a local model (ONNX), an external API (OpenAI, Voyage),
//! or a simple hash-based function for testing. This module handles
//! chunking, storage, staleness detection, and search.

use super::*;
use atomic_core::pristine::vault::{EmbeddingRecord, SearchResult, VaultEntryType};
use atomic_core::pristine::{EmbeddingsMutTxnT, EmbeddingsTxnT};

/// Configuration for the embedding pipeline.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// Maximum tokens per chunk (approximate, based on chars/4).
    pub max_chunk_tokens: usize,
    /// Embedding dimensions.
    pub dimensions: usize,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            max_chunk_tokens: 512,
            dimensions: 384, // MiniLM default
        }
    }
}

impl Repository {
    /// Initialize the embeddings table.
    pub fn init_embeddings(&self) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.init_embeddings()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Embed a single vault entry using the provided embedding function.
    ///
    /// Chunks the content by heading/paragraph, computes embeddings via `embed_fn`,
    /// and stores them in redb. Returns the number of chunks stored.
    ///
    /// Staleness detection: skips re-embedding if the content hash matches
    /// the existing stored hash.
    pub fn vault_embed<F>(
        &self,
        path: &str,
        embed_fn: &F,
        config: &EmbedConfig,
    ) -> Result<usize, RepositoryError>
    where
        F: Fn(&str) -> Vec<f32>,
    {
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound {
                path: std::path::PathBuf::from(path),
            })?;

        let content = String::from_utf8_lossy(&entry.content_bytes);
        let content_hash = entry.content_hash;

        // Check staleness — if first chunk has same hash, skip
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Ok(Some(existing)) = txn.get_embedding(path, 0) {
                if existing.content_hash == content_hash {
                    return Ok(0); // Content unchanged, skip
                }
            }
        }

        // Chunk the content
        let chunks = chunk_by_heading(&content, config.max_chunk_tokens);

        // Delete old embeddings for this path
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let _ = txn
                .del_embeddings(path)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Embed and store each chunk
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut count = 0;
        for (idx, chunk) in chunks.iter().enumerate() {
            if chunk.text.trim().is_empty() {
                continue;
            }

            let vector = embed_fn(&chunk.text);
            let preview = chunk.text.chars().take(200).collect::<String>();

            let record = EmbeddingRecord {
                vector,
                content_hash,
                introduced_by: entry.introduced_by,
                chunk_idx: idx as u32,
                preview,
            };

            txn.put_embedding(path, idx as u32, &record)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            count += 1;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(count)
    }

    /// Embed all vault entries.
    pub fn vault_embed_all<F>(
        &self,
        embed_fn: &F,
        config: &EmbedConfig,
    ) -> Result<usize, RepositoryError>
    where
        F: Fn(&str) -> Vec<f32>,
    {
        let entries = self.vault_list("", None)?;
        let mut total = 0;
        for meta in &entries {
            // Only embed text content types
            match meta.entry_type {
                VaultEntryType::Memory
                | VaultEntryType::Intent
                | VaultEntryType::Session
                | VaultEntryType::Skill => match self.vault_embed(&meta.path, embed_fn, config) {
                    Ok(count) => total += count,
                    Err(e) => log::warn!("Failed to embed {}: {}", meta.path, e),
                },
                _ => {}
            }
        }
        Ok(total)
    }

    /// Search vault embeddings for similar content.
    pub fn vault_search(
        &self,
        query_vector: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.search_embeddings(query_vector, top_k)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count total embeddings stored.
    pub fn vault_embedding_count(&self) -> Result<usize, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.count_embeddings()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }
}

// ── Chunking ────────────────────────────────────────────────────

/// A chunk of text with its position.
#[derive(Debug, Clone)]
pub struct TextChunk {
    /// The chunk text content.
    pub text: String,
    /// Starting line number (0-based) within the source document.
    pub start_line: usize,
}

/// Chunk text by heading boundaries, then by paragraph, respecting
/// a maximum token budget per chunk.
///
/// Strategy:
/// 1. Split on `#` headings (any level)
/// 2. If a section exceeds max_tokens, split by paragraph (blank lines)
/// 3. If a paragraph still exceeds, split by lines
fn chunk_by_heading(content: &str, max_tokens: usize) -> Vec<TextChunk> {
    let max_chars = max_tokens * 4; // rough char-to-token ratio
    let mut chunks = Vec::new();
    let mut current_text = String::new();
    let mut current_start = 0;

    for (line_idx, line) in content.lines().enumerate() {
        // Check if this line is a heading
        let is_heading = line.starts_with('#') && line.chars().find(|&c| c != '#') == Some(' ');

        if is_heading && !current_text.is_empty() {
            // Flush current chunk
            flush_chunk(&mut chunks, &current_text, current_start, max_chars);
            current_text.clear();
            current_start = line_idx;
        }

        if !current_text.is_empty() {
            current_text.push('\n');
        }
        current_text.push_str(line);
    }

    // Flush final chunk
    if !current_text.is_empty() {
        flush_chunk(&mut chunks, &current_text, current_start, max_chars);
    }

    chunks
}

/// Flush a text section as one or more chunks, splitting if too large.
fn flush_chunk(chunks: &mut Vec<TextChunk>, text: &str, start_line: usize, max_chars: usize) {
    if text.len() <= max_chars {
        chunks.push(TextChunk {
            text: text.to_string(),
            start_line,
        });
        return;
    }

    // Split by paragraph (blank lines)
    let mut current = String::new();
    let mut line_offset = 0;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() && !current.is_empty() && current.len() > max_chars / 2 {
            chunks.push(TextChunk {
                text: current.clone(),
                start_line: start_line + line_offset,
            });
            current.clear();
            line_offset = i + 1;
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);

        // Hard split if still too large
        if current.len() > max_chars {
            chunks.push(TextChunk {
                text: current.clone(),
                start_line: start_line + line_offset,
            });
            current.clear();
            line_offset = i + 1;
        }
    }
    if !current.is_empty() {
        chunks.push(TextChunk {
            text: current,
            start_line: start_line + line_offset,
        });
    }
}

/// Simple hash-based "embedding" for testing purposes.
///
/// Produces a deterministic vector from text content using character
/// frequency analysis. NOT suitable for production — use a real
/// embedding model (MiniLM, OpenAI, etc.) instead.
pub fn hash_embed(text: &str, dims: usize) -> Vec<f32> {
    let mut vec = vec![0.0f32; dims];
    for (i, byte) in text.bytes().enumerate() {
        vec[i % dims] += byte as f32 / 255.0;
    }
    // Normalize
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }
    vec
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::vault::VaultEntryType;

    fn test_embed(text: &str) -> Vec<f32> {
        hash_embed(text, 8) // tiny dims for tests
    }

    #[test]
    fn test_chunk_by_heading() {
        let content = "# Heading 1\n\nParagraph one.\n\n# Heading 2\n\nParagraph two.\n";
        let chunks = chunk_by_heading(content, 512);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("Heading 1"));
        assert!(chunks[1].text.contains("Heading 2"));
    }

    #[test]
    fn test_chunk_no_headings() {
        let content = "Just some text\nwith multiple lines\nbut no headings.";
        let chunks = chunk_by_heading(content, 512);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunk_large_section_split() {
        // Use newline-separated lines so flush_chunk can split by paragraph/line
        let content = format!("# Big Section\n\n{}", "word\n".repeat(1000));
        let chunks = chunk_by_heading(&content, 50); // very small budget
        assert!(chunks.len() > 1);
    }

    #[test]
    fn test_hash_embed_deterministic() {
        let v1 = hash_embed("hello world", 8);
        let v2 = hash_embed("hello world", 8);
        assert_eq!(v1, v2);
    }

    #[test]
    fn test_hash_embed_normalized() {
        let v = hash_embed("test content", 8);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_hash_embed_different_content() {
        let v1 = hash_embed("hello", 8);
        let v2 = hash_embed("world", 8);
        assert_ne!(v1, v2);
    }

    #[test]
    fn test_vault_embed_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        repo.init_embeddings().unwrap();

        // Store some entries
        repo.vault_store(
            "memory/auth.md",
            VaultEntryType::Memory,
            b"# Authentication\n\nOAuth2 implementation details.\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_store(
            "memory/testing.md",
            VaultEntryType::Memory,
            b"# Testing\n\nUnit test best practices.\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        // vault_store auto-embeds; manual embed returns 0 (stale)
        let config = EmbedConfig {
            max_chunk_tokens: 512,
            dimensions: 8,
        };
        let c1 = repo
            .vault_embed("memory/auth.md", &test_embed, &config)
            .unwrap();
        assert_eq!(c1, 0, "manual embed should skip (auto-embed already ran)");

        // Count — auto-embed populated these
        assert!(repo.vault_embedding_count().unwrap() >= 2);

        // Search — auto-embed uses hash_embed with provider dims (384 for Local).
        // Use a large top_k so default vault content doesn't push test entries out.
        let provider = crate::ai::resolve_embedding_provider();
        let query = hash_embed("authentication oauth2", provider.dimensions);
        let results = repo.vault_search(&query, 50).unwrap();
        assert!(!results.is_empty());
        // Both entries should appear somewhere in results (hash_embed is a
        // placeholder, so we don't assert ranking — a real model would rank
        // auth.md higher)
        let paths: Vec<&str> = results.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"memory/auth.md"));
        assert!(paths.contains(&"memory/testing.md"));
    }

    #[test]
    fn test_vault_embed_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        repo.init_embeddings().unwrap();

        repo.vault_store(
            "memory/test.md",
            VaultEntryType::Memory,
            b"# Test\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let config = EmbedConfig {
            max_chunk_tokens: 512,
            dimensions: 8,
        };

        // Auto-embed already ran during vault_store; verify it populated
        assert!(
            repo.vault_embedding_count().unwrap() > 0,
            "auto-embed should have stored at least one chunk"
        );

        // Manual embed — same content, should skip (stale)
        let c1 = repo
            .vault_embed("memory/test.md", &test_embed, &config)
            .unwrap();
        assert_eq!(c1, 0, "should skip — content unchanged since auto-embed");

        // Second manual embed — still stale
        let c2 = repo
            .vault_embed("memory/test.md", &test_embed, &config)
            .unwrap();
        assert_eq!(c2, 0);
    }

    #[test]
    fn test_vault_embed_all() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        repo.init_embeddings().unwrap();

        repo.vault_store(
            "memory/a.md",
            VaultEntryType::Memory,
            b"# A\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_store(
            "memory/b.md",
            VaultEntryType::Memory,
            b"# B\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let config = EmbedConfig {
            max_chunk_tokens: 512,
            dimensions: 8,
        };
        // Auto-embed already ran for each vault_store; embed_all returns 0 (all stale)
        let total = repo.vault_embed_all(&test_embed, &config).unwrap();
        assert_eq!(total, 0, "all entries already auto-embedded");

        // But embeddings do exist from auto-embed
        assert!(repo.vault_embedding_count().unwrap() >= 2);
    }
}
