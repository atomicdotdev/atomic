//! Change identifier parsing and resolution.
//!
//! Accepts the same identifier forms as `atomic change`/`atomic log`:
//! a full 52-character base32 hash, a hash prefix (≥ 4 chars), a sequence
//! number (`#12` or `12`), `@` for the latest change, or no identifier at
//! all (also latest).

use atomic_core::pristine::{ViewState, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{find_change_sequence, get_change_at_sequence, HistoryError, Repository};

use crate::error::{FacadeError, FacadeResult};

/// A parsed change identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeIdentifier {
    /// Full 52-character base32 hash.
    FullHash(Hash),
    /// Hash prefix (4–51 characters, uppercased).
    HashPrefix(String),
    /// Sequence number in a view's history.
    Sequence(u64),
    /// No identifier — the most recent change on the view.
    Latest,
}

impl ChangeIdentifier {
    /// Parse an identifier string.
    ///
    /// `None`, `""`, and `"@"` all mean [`ChangeIdentifier::Latest`]. A
    /// `#`-prefixed or purely numeric string is a sequence number. A
    /// 52-character base32 string is a full hash; 4–51 base32 characters
    /// are a prefix.
    pub fn parse(spec: Option<&str>) -> FacadeResult<Self> {
        let s = match spec {
            None | Some("") => return Ok(Self::Latest),
            Some(s) => s.trim(),
        };

        if s == "@" {
            return Ok(Self::Latest);
        }

        if let Some(num) = s.strip_prefix('#') {
            return num.parse::<u64>().map(Self::Sequence).map_err(|_| {
                FacadeError::InvalidIdentifier {
                    message: format!("invalid sequence number: {num}"),
                }
            });
        }

        if s.chars().all(|c| c.is_ascii_digit()) {
            return s.parse::<u64>().map(Self::Sequence).map_err(|_| {
                FacadeError::InvalidIdentifier {
                    message: format!("invalid sequence number: {s}"),
                }
            });
        }

        let upper = s.to_uppercase();
        if !upper
            .chars()
            .all(|c| "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".contains(c))
        {
            return Err(FacadeError::InvalidIdentifier {
                message: format!(
                    "invalid hash characters in '{s}' — hashes use base32 (A-Z, 2-7)"
                ),
            });
        }

        if upper.len() == 52 {
            return Hash::from_base32(upper.as_bytes())
                .map(Self::FullHash)
                .ok_or_else(|| FacadeError::InvalidIdentifier {
                    message: format!("invalid base32 hash: {s}"),
                });
        }

        if upper.len() >= 4 {
            Ok(Self::HashPrefix(upper))
        } else {
            Err(FacadeError::InvalidIdentifier {
                message: format!("hash prefix too short: '{s}' (minimum 4 characters)"),
            })
        }
    }
}

/// Resolve a change identifier to a `(hash, sequence)` pair on a view.
///
/// `view` defaults to the repository's current view. The returned sequence is
/// `None` when the change exists in the store but is not on the view's log.
pub fn resolve_change(
    repo: &Repository,
    view: Option<&str>,
    spec: Option<&str>,
) -> FacadeResult<(Hash, Option<u64>)> {
    let id = ChangeIdentifier::parse(spec)?;
    let view_name = view.unwrap_or_else(|| repo.current_view());

    match id {
        ChangeIdentifier::FullHash(hash) => {
            if !repo.has_change(&hash) {
                return Err(FacadeError::ChangeNotFound {
                    id: hash.to_base32(),
                });
            }
            let seq = sequence_for_hash(repo, view_name, &hash)?;
            Ok((hash, seq))
        }
        ChangeIdentifier::HashPrefix(prefix) => resolve_prefix(repo, view_name, &prefix),
        ChangeIdentifier::Sequence(seq) => {
            let hash = resolve_sequence(repo, view_name, seq)?;
            Ok((hash, Some(seq)))
        }
        ChangeIdentifier::Latest => latest_change(repo, view_name),
    }
}

fn sequence_for_hash(
    repo: &Repository,
    view_name: &str,
    hash: &Hash,
) -> FacadeResult<Option<u64>> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| FacadeError::Repository(atomic_repository::RepositoryError::Database(
            e.to_string(),
        )))?;
    let view = get_view(&txn, view_name)?;
    find_change_sequence(&txn, &view, hash).map_err(history_error)
}

fn resolve_prefix(
    repo: &Repository,
    view_name: &str,
    prefix: &str,
) -> FacadeResult<(Hash, Option<u64>)> {
    let mut matches: Vec<Hash> = Vec::new();
    for result in repo.iter_changes() {
        let hash = result?;
        if hash.to_base32().starts_with(prefix) {
            matches.push(hash);
        }
    }

    match matches.as_slice() {
        [] => Err(FacadeError::ChangeNotFound {
            id: prefix.to_string(),
        }),
        [hash] => {
            let seq = sequence_for_hash(repo, view_name, hash)?;
            Ok((*hash, seq))
        }
        many => Err(FacadeError::Ambiguous {
            prefix: prefix.to_string(),
            matches: many.iter().map(Hash::to_base32).collect(),
        }),
    }
}

fn resolve_sequence(repo: &Repository, view_name: &str, seq: u64) -> FacadeResult<Hash> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| FacadeError::Repository(atomic_repository::RepositoryError::Database(
            e.to_string(),
        )))?;
    let view = get_view(&txn, view_name)?;
    let entry = get_change_at_sequence(&txn, &view, seq).map_err(|e| match e {
        HistoryError::SequenceOutOfRange { sequence, max } => FacadeError::InvalidIdentifier {
            message: format!(
                "sequence {sequence} out of range — view has {} changes (0-{max})",
                max + 1
            ),
        },
        other => history_error(other),
    })?;
    Ok(entry.hash)
}

fn latest_change(repo: &Repository, view_name: &str) -> FacadeResult<(Hash, Option<u64>)> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| FacadeError::Repository(atomic_repository::RepositoryError::Database(
            e.to_string(),
        )))?;
    let view = get_view(&txn, view_name)?;

    if view.change_count == 0 {
        return Err(FacadeError::ChangeNotFound {
            id: format!("latest change on empty view '{view_name}'"),
        });
    }

    let seq = view.change_count - 1;
    let entry = get_change_at_sequence(&txn, &view, seq).map_err(history_error)?;
    Ok((entry.hash, Some(seq)))
}

fn get_view<T: ViewTxnT>(txn: &T, view_name: &str) -> FacadeResult<ViewState> {
    txn.get_view(view_name)
        .map_err(|e| {
            FacadeError::Repository(atomic_repository::RepositoryError::Database(e.to_string()))
        })?
        .ok_or_else(|| FacadeError::ViewNotFound {
            name: view_name.to_string(),
        })
}

fn history_error(e: HistoryError) -> FacadeError {
    FacadeError::Repository(atomic_repository::RepositoryError::Database(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_latest_forms() {
        for spec in [None, Some(""), Some("@")] {
            assert_eq!(ChangeIdentifier::parse(spec).unwrap(), ChangeIdentifier::Latest);
        }
    }

    #[test]
    fn parse_sequence_forms() {
        assert_eq!(
            ChangeIdentifier::parse(Some("#7")).unwrap(),
            ChangeIdentifier::Sequence(7)
        );
        assert_eq!(
            ChangeIdentifier::parse(Some("42")).unwrap(),
            ChangeIdentifier::Sequence(42)
        );
    }

    #[test]
    fn parse_prefix_uppercases() {
        assert_eq!(
            ChangeIdentifier::parse(Some("abcd")).unwrap(),
            ChangeIdentifier::HashPrefix("ABCD".to_string())
        );
    }

    #[test]
    fn parse_rejects_short_prefix_and_bad_chars() {
        assert!(ChangeIdentifier::parse(Some("abc")).is_err());
        assert!(ChangeIdentifier::parse(Some("not-a-hash!")).is_err());
    }
}
