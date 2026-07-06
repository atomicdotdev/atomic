//! `atomic provenance {trace,show}` subcommand implementations.
//!
//! Both are READ-ONLY and COMPUTE-ON-DEMAND: they load the per-turn
//! `ProvenanceGraph` atomic already captured and project it into a W3C PROV
//! JSON-LD named subgraph. Per the baseline the graph is **signable, not signed**
//! — the default JSON output is the unsigned projection; `--sign` produces the
//! signable standalone artifact (a top-level `attributedTo`/`contentHash`/`proof`
//! bound to the person's identity) for handing to an auditor. NOTHING is written
//! either way — no vault entry, no sidecar, no `save_provenance_graph`, no
//! content-hash/merkle touch.

use clap::Parser;

use atomic_canonical::did::did_for_public_key;
use atomic_canonical::prov::{activity_urn, attest_prov, project, ProvActivityInput};
use atomic_core::change::ProvenanceGraph;
use atomic_core::types::{Base32, Hash};
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::provenance::mapping::{activity_id_for, map_graph_to_input};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint, info};

/// Show the flywheel chain for a change.
///
/// Walks the provenance graph for a change — the activity that generated it, the
/// changes it generated, the agent and person involved, and the parent turn —
/// and prints the human-readable chain. `--json` emits the PROV JSON-LD named
/// subgraph instead (unsigned by default; add `--sign` for the signable artifact).
#[derive(Parser, Debug)]
#[command(name = "trace")]
pub struct ProvenanceTrace {
    /// Change hash, hash prefix, or `urn:atomic:change:<base32>`.
    pub target: String,

    /// Identity whose key signs the projection (with `--json --sign`, and to
    /// resolve the Person). Defaults to the current default identity.
    #[arg(long)]
    pub identity: Option<String>,

    /// Emit the PROV JSON-LD `@graph` instead of the human chain.
    #[arg(long)]
    pub json: bool,

    /// With `--json`, emit the SIGNABLE artifact — sign the graph (top-level
    /// `attributedTo`/`contentHash`/`proof`) instead of the plain projection.
    #[arg(long)]
    pub sign: bool,
}

/// Project the PROV JSON-LD named subgraph for a change.
///
/// Prints the UNSIGNED projection by default — the baseline's "signable" unit,
/// a derived view whose integrity comes from `turnParent` hash-linking and the
/// already-signed primaries it references. `--sign` emits the SIGNABLE standalone
/// artifact instead: a top-level `attributedTo`/`contentHash`/`proof` envelope
/// injected by the shared signing path (the person's `did:atomic` signs the whole
/// subgraph), for handing to an auditor as a self-verifying bundle.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct ProvenanceShow {
    /// Change hash, hash prefix, or `urn:atomic:change:<base32>`.
    pub target: String,

    /// Identity used to resolve the Person (and to sign with `--sign`). Defaults
    /// to the current default identity.
    #[arg(long)]
    pub identity: Option<String>,

    /// Emit the SIGNABLE artifact — sign the graph instead of the plain projection.
    #[arg(long)]
    pub sign: bool,
}

impl Command for ProvenanceTrace {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let change_hash = resolve_change_target(&repo, &self.target)?;
        let graphs = load_graphs(&repo, &change_hash)?;

        if self.json {
            // Emit the JSON-LD, exactly like `show` (unsigned by default; `--sign`
            // for the signable artifact).
            let (identity, keypair) = resolve_person(self.identity.as_deref())?;
            let person_did = did_for_public_key(&identity.public_key);
            let values = graphs
                .iter()
                .map(|(_, g)| {
                    let input = map_graph_to_input(&repo, g, &change_hash, &person_did);
                    if self.sign {
                        attest_prov(&input, &identity, &keypair)
                    } else {
                        project(&input)
                    }
                })
                .collect::<Vec<_>>();
            print_value(&values, self.sign);
            return Ok(());
        }

        // Plain trace: no identity/key required. Use the graph's own person
        // slot as "(unresolved)" — the human chain does not need a signature.
        // We still resolve a Person did if one is available so the chain shows
        // the real signer; otherwise the actedOnBehalfOf target is left blank.
        let person_did = resolve_person(self.identity.as_deref())
            .ok()
            .map(|(id, _)| did_for_public_key(&id.public_key))
            .unwrap_or_else(|| "(no signing identity)".to_string());

        print_trace(&repo, &graphs, &change_hash, &person_did);
        Ok(())
    }
}

impl Command for ProvenanceShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let change_hash = resolve_change_target(&repo, &self.target)?;
        let graphs = load_graphs(&repo, &change_hash)?;

        let (identity, keypair) = resolve_person(self.identity.as_deref())?;
        let person_did = did_for_public_key(&identity.public_key);

        let values = graphs
            .iter()
            .map(|(_, g)| {
                let input = map_graph_to_input(&repo, g, &change_hash, &person_did);
                if self.sign {
                    attest_prov(&input, &identity, &keypair)
                } else {
                    project(&input)
                }
            })
            .collect::<Vec<_>>();

        print_value(&values, self.sign);
        Ok(())
    }
}

/// Load a change's provenance graphs: REV_DEPS-backed lookup, with a disk-scan
/// fallback when REV_DEPS registration missed the change. Errors if no graph
/// explains the change.
fn load_graphs(
    repo: &Repository,
    change_hash: &Hash,
) -> CliResult<Vec<(Hash, ProvenanceGraph)>> {
    let mut graphs = repo
        .find_provenance_for_change(change_hash)
        .map_err(CliError::Repository)?;
    if graphs.is_empty() {
        graphs = repo
            .find_provenance_for_change_scan(change_hash)
            .map_err(CliError::Repository)?;
    }
    if graphs.is_empty() {
        return Err(CliError::InvalidArgument {
            message: format!(
                "no provenance graph explains change {}",
                change_hash.to_base32()
            ),
        });
    }
    // Most-recent first (a change explained by >1 graph shows all; the newest
    // leads). Both loaders sort/return unspecified order, so sort here.
    graphs.sort_by(|(_, a), (_, b)| b.timestamp.cmp(&a.timestamp));
    Ok(graphs)
}

/// Resolve the CLI target (bare hash, hash prefix, or `urn:atomic:change:<b32>`)
/// to a change `Hash`.
fn resolve_change_target(repo: &Repository, target: &str) -> CliResult<Hash> {
    const URN_PREFIX: &str = "urn:atomic:change:";
    if let Some(b32) = target.strip_prefix(URN_PREFIX) {
        return Hash::from_base32(b32.as_bytes()).ok_or_else(|| CliError::InvalidArgument {
            message: format!("invalid change base32 in URN: {target}"),
        });
    }

    // Bare full hash or prefix: match against the change store by base32 prefix
    // (mirrors ChangeCmd::resolve_hash_prefix).
    let mut matches: Vec<Hash> = Vec::new();
    for result in repo.iter_changes() {
        let hash = result.map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;
        if hash.to_base32().starts_with(target) {
            matches.push(hash);
        }
    }
    match matches.len() {
        0 => {
            // No recorded change matched. A graph can explain a change that was
            // never recorded/made internal (the REV_DEPS fallback case), so try
            // parsing the target as a full base32 change hash directly.
            Hash::from_base32(target.as_bytes()).ok_or_else(|| CliError::ChangeNotFound {
                hash: target.to_string(),
            })
        }
        1 => Ok(matches[0]),
        _ => {
            let list: Vec<String> = matches.iter().map(|h| h.to_base32()).collect();
            Err(CliError::AmbiguousHash {
                hash: format!("{} (matches: {})", target, list.join(", ")),
            })
        }
    }
}

/// Resolve the PERSON's signing identity + keypair, exactly as
/// `atomic intent attest` does.
fn resolve_person(
    identity: Option<&str>,
) -> CliResult<(atomic_identity::identity::Identity, atomic_identity::keypair::KeyPair)> {
    let store = IdentityStore::open_default().map_err(|e| {
        CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
    })?;
    let identity = if let Some(name) = identity {
        store
            .load_by_name(name)
            .map_err(|_| CliError::IdentityNotFound(name.to_string()))?
    } else {
        store
            .get_default()
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to load default identity: {}", e))
            })?
            .ok_or_else(|| CliError::InvalidArgument {
                message: "No default identity set. Create one first:\n  \
                          atomic identity new <name> --email <email> --set-default"
                    .to_string(),
            })?
    };
    let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
        CliError::Internal(anyhow::anyhow!(
            "Failed to load keypair for '{}': {}",
            identity.name,
            e
        ))
    })?;
    Ok((identity, keypair))
}

/// Print the PROV JSON-LD artifact(s) to stdout. When `signed`, add the
/// dev-signature caveat (the signing path was used).
fn print_value(values: &[serde_json::Value], signed: bool) {
    let out = if values.len() == 1 {
        serde_json::to_string_pretty(&values[0])
    } else {
        serde_json::to_string_pretty(&values)
    }
    .expect("PROV value serialization is infallible");
    println!("{out}");
    if signed {
        eprintln!(
            "note: signing keys are stored unencrypted on disk; treat this \
             signed projection as a non-production dev signature until key-at-rest \
             encryption lands."
        );
    }
}

/// Print the human-readable flywheel chain: change -> activity -> generated ->
/// agent -> person -> turnParent -> ... (walking `previous`).
fn print_trace(
    repo: &Repository,
    graphs: &[(Hash, ProvenanceGraph)],
    change_hash: &Hash,
    person_did: &str,
) {
    println!(
        "{} {}",
        emphasis("Provenance for change"),
        info(&change_hash.to_base32())
    );
    for (i, (_, graph)) in graphs.iter().enumerate() {
        if graphs.len() > 1 {
            println!("{}", hint(&format!("  [graph {} of {}]", i + 1, graphs.len())));
        }
        let input = map_graph_to_input(repo, graph, change_hash, person_did);
        print_activity_chain(repo, &input, graph);
    }
}

/// Print one activity and walk its `turnParent` chain via `previous`.
fn print_activity_chain(repo: &Repository, input: &ProvActivityInput, graph: &ProvenanceGraph) {
    // The projected (unsigned) value is the source of truth for the shape.
    let value = project(input);
    let activity = value
        .get("@graph")
        .and_then(|g| g.as_array())
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|n| n.get("@type").and_then(|t| t.as_str()) == Some("prov:Activity"))
        });

    let indent = "  ";
    if let Some(act) = activity {
        println!(
            "{indent}{} {}",
            emphasis("activity"),
            info(act.get("@id").and_then(|v| v.as_str()).unwrap_or("?"))
        );
        if let Some(gen) = act.get("generated").and_then(|v| v.as_array()) {
            for g in gen {
                println!(
                    "{indent}  {} {}",
                    hint("generated"),
                    info(g.as_str().unwrap_or("?"))
                );
            }
        }
        println!(
            "{indent}  {} {}",
            hint("agent"),
            info(act.get("associatedWith").and_then(|v| v.as_str()).unwrap_or("?"))
        );
        println!(
            "{indent}  {} {} ({})",
            hint("agent label"),
            info(&input.agent_display_name),
            input.agent_vendor.as_deref().unwrap_or("no vendor")
        );
        println!(
            "{indent}  {} {}",
            hint("person"),
            info(act.get("actedOnBehalfOf").and_then(|v| v.as_str()).unwrap_or("?"))
        );
        if let Some(parent) = act.get("turnParent").and_then(|v| v.as_str()) {
            println!("{indent}  {} {}", hint("turnParent"), info(parent));
        } else {
            println!("{indent}  {}", hint("turnParent: (root turn)"));
        }
    }

    // Walk `previous` to render the prior turn (one level; recursion via loop).
    let mut cursor = graph.previous;
    let mut depth = 0usize;
    while let Some(prev_hash) = cursor {
        depth += 1;
        if depth > 64 {
            println!("{indent}  {}", hint("(chain truncated)"));
            break;
        }
        match repo.load_provenance_graph(&prev_hash) {
            Ok(prev) => {
                println!(
                    "{indent}  {} activity {}",
                    hint("↑ prior turn"),
                    activity_urn(&activity_id_for(&prev)),
                );
                cursor = prev.previous;
            }
            Err(_) => {
                println!("{indent}  {}", hint("↑ prior turn (graph unavailable)"));
                break;
            }
        }
    }
}
