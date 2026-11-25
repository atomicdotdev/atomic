//! SHACL plumbing shared by `kind validate` and `query validate --profile kind`.
//!
//! The custom-kind gate is the same in both commands: compile each recorded
//! `_kind.md` spec into a generated `NodeShape`, then run it over the RDF
//! projection of the kind's instances. Keeping the two callers on one helper
//! means a server-side `query validate` cannot drift from the CLI gate.
//!
//! The dataset is projected from the same source the shapes target — the
//! working copy or a named view — not from the ambient knowledge graph. The
//! ambient projection can be stale or hold a different view's rows, and the
//! gate must validate the rows it reports on. See ADR 0001 decision 2.

use atomic_core::Base32;
use atomic_repository::{KindPath, KindSource, Repository};

use crate::error::{CliError, CliResult};

/// The generated shapes for the selected kinds and the instances they target.
pub(crate) struct KindShapes {
    /// One Turtle `NodeShape` per registered kind.
    pub bundles: Vec<atomic_shacl::ShapeBundle>,
    /// `(kind, uid)` for every instance the selected kinds track.
    pub instances: Vec<(String, String)>,
    /// Where the rows came from, so the dataset projects the same files.
    pub source: KindSource,
    /// The single kind name selected, if any.
    pub only: Option<String>,
}

/// Compile the selected kinds' specs into SHACL shapes.
///
/// `only` limits the work to one kind name. A kind whose definition is not
/// registered contributes no shape: the field-level gate reports the broken
/// definition and there is nothing to compile.
pub(crate) fn kind_shapes(
    repo: &Repository,
    source: &KindSource,
    only: Option<&str>,
) -> CliResult<KindShapes> {
    let registry = repo.kind_registry(source).map_err(CliError::Repository)?;
    let project =
        atomic_core::kg::iri::mint_project(&repo.project_id().map_err(CliError::Repository)?);

    let mut bundles = Vec::new();
    let mut instances = Vec::new();
    for entry in registry.kinds() {
        if only.is_some_and(|name| name != entry.name()) {
            continue;
        }
        let Some(spec) = entry.spec() else {
            continue;
        };
        let turtle = atomic_shacl::kind_shapes::shape_for(spec, &project)
            .map_err(|error| CliError::ShaclError(error.to_string()))?;
        bundles.push(atomic_shacl::ShapeBundle {
            iri: format!("{project}:kind:{}#Shape", spec.name),
            turtle,
        });
        for path in &entry.instances {
            if let Some(KindPath::Instance { uid, .. }) = KindPath::classify(path) {
                instances.push((spec.name.clone(), uid));
            }
        }
    }
    Ok(KindShapes {
        bundles,
        instances,
        source: source.clone(),
        only: only.map(str::to_owned),
    })
}

/// The RDF dataset and the instances it cannot express.
///
/// The dataset is projected from the selected source's kind rows. `missing`
/// lists instances the source tracks but the projection cannot express (an
/// unresolved name conflict, a definition that does not register): the gate
/// fails closed rather than passing vacuously.
pub(crate) struct KindDataset {
    pub ntriples: String,
    pub snapshot_identity: String,
    pub missing: Vec<String>,
}

pub(crate) fn kind_dataset(repo: &Repository, shapes: &KindShapes) -> CliResult<KindDataset> {
    let (nodes, edges) = repo
        .kind_kg_graph(&shapes.source, shapes.only.as_deref())
        .map_err(CliError::Repository)?;

    let mut missing = Vec::new();
    for (kind, uid) in &shapes.instances {
        // No project identity means no IRI to project; the field-level gate
        // remains the authority for that repository.
        let Some(iri) = repo
            .kind_instance_iri(kind, uid)
            .map_err(CliError::Repository)?
        else {
            continue;
        };
        if !nodes.iter().any(|node| node.id == iri) {
            missing.push(format!("{kind}:{uid}"));
        }
    }

    let ntriples = atomic_shacl::snapshot_ntriples(&nodes, &edges)
        .map_err(|error| CliError::ShaclError(error.to_string()))?;
    let snapshot_identity = atomic_core::Hash::of(ntriples.as_bytes()).to_base32();
    Ok(KindDataset {
        ntriples,
        snapshot_identity,
        missing,
    })
}
