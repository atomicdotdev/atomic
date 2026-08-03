//! `atomic intent new <TITLE> [--template feature] [--kind KIND | --review TARGET]`
//! — scaffold a directive-based intent into the vault.

use clap::Parser;

use atomic_canonical::vocab::{is_known_intent_kind, INTENT_KIND};
use atomic_repository::{IntentCreateOptions, IntentCreateResult, IntentUpdateOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// The directive scaffold emitted for a new intent.
///
/// This is DISTINCT from the legacy positional template shipped at
/// `atomic-repository/vault/templates/intent.md` (Problem / `- [ ]` checkboxes /
/// Scope headings), which is not directive-based and lifts to nothing. This
/// scaffold is built from the closed directive vocabulary the lift understands
/// (`atomic-canonical/src/lift.rs`: `why`, `acceptance-criterion`, `scope-in`,
/// `scope-out`, `constraint`) so it round-trips cleanly through
/// lift → validate → attest.
///
/// `{id}` is replaced with the allocated intent ID so the acceptance-criterion
/// gets a stable, prefixed local id. A `:::why` stub is mandatory (the gate
/// hard-requires a non-empty `why`), and because it emits a `:::scope-in` it
/// must also emit a `:::scope-out` (the gate requires the out-of-scope boundary
/// whenever scope is declared).
const FEATURE_SCAFFOLD: &str = "\
:::why
<!-- Why does this intent exist? State the reason this work matters. The
     content is never graded — but it must be present. Replace this stub. -->
:::

:::acceptance-criterion{#{id}-ac-1 status=unmet}
<!-- A single, checkable outcome that means this intent is done. -->
:::

:::task{#{id}-1 status=open criteria={id}-ac-1}
<!-- A concrete work item toward the criterion above. Name the file(s) it
     touches with one or more ::file-ref leaves. -->
::file-ref{path=path/to/file}
:::

:::scope-in
<!-- What this intent will change. -->
:::

:::scope-out
<!-- What this intent will deliberately NOT change (the boundaries the agent
     must respect). -->
:::

:::constraint
<!-- A rule the implementation must respect. -->
:::
";

/// The directive scaffold emitted for a `--review <TARGET>` intent.
///
/// Identical in shape to [`FEATURE_SCAFFOLD`] but carries a
/// `:::ref{to=<TARGET> edge=reviews}` leaf. The gate's `ReviewShape` couples the
/// `review` kind to the `reviews` edge (kind==review ⟺ exactly-a-reviews-edge),
/// so seeding the ref here makes a `--review` intent conform out of the box. The
/// acceptance-criterion stub is phrased as a review verdict.
///
/// `{id}` is replaced with the intent's ULID (child-id namespacing) and
/// `{target}` with the reviewed intent reference.
const REVIEW_SCAFFOLD: &str = "\
:::why
<!-- Why does this review matter? State what makes reviewing the target work
     worthwhile. The content is never graded — but it must be present. -->
:::

:::acceptance-criterion{#{id}-ac-1 status=unmet}
<!-- Review verdict: the reviewed work is correct, tested, in scope, and its
     own acceptance criteria are genuinely met. A single checkable outcome. -->
:::

:::task{#{id}-1 status=open criteria={id}-ac-1}
<!-- Review the target intent's work and record the findings. -->
::file-ref{path=path/to/file}
:::

:::ref{to={target} edge=reviews}
:::

:::scope-in
<!-- What this review covers. -->
:::

:::scope-out
<!-- What this review deliberately does NOT cover. -->
:::

:::constraint
<!-- A rule this review must respect. -->
:::
";

/// Scaffold a new directive-based intent into the vault.
#[derive(Parser, Debug)]
#[command(name = "new")]
pub struct IntentNew {
    /// The intent's title (a short summary of the work). NOTE: the human key /
    /// id (e.g. PIMO-1) is allocated by the vault — this positional is the title,
    /// not the id.
    pub title: String,

    /// Scaffold template to emit. Only "feature" is implemented.
    #[arg(long, default_value = "feature")]
    pub template: String,

    /// Classification of the intent (one of feature, review, bug, chore,
    /// remediation). Threaded into the intent's `kind:` frontmatter. Mutually
    /// exclusive with `--review` (which forces `kind=review`).
    #[arg(long, default_value = "feature", conflicts_with = "review")]
    pub kind: String,

    /// Author a *review* intent that reviews TARGET. Implies `kind=review` and
    /// scaffolds a `:::ref{to=<TARGET> edge=reviews}` leaf so the intent
    /// conforms to the gate's ReviewShape immediately. TARGET is the reviewed
    /// intent reference (e.g. `urn:atomic:intent:PIMO-1`).
    #[arg(long, value_name = "TARGET")]
    pub review: Option<String>,
}

/// The core create logic, factored out so it can be unit-tested against an
/// explicit [`Repository`] (the [`Command::run`] impl resolves the repo from the
/// cwd and delegates here). Returns the created intent alongside its resolved
/// classification `kind` (for display).
///
/// `--review` wins over `--kind`: when `review_target` is `Some`, the intent is
/// a `review` regardless of `kind`. The CLI parser already rejects supplying
/// both explicitly (`conflicts_with`), so this only encodes the precedence.
fn create_intent(
    repo: &Repository,
    title: &str,
    kind: &str,
    review_target: Option<&str>,
) -> CliResult<(IntentCreateResult, String)> {
    // Resolve the effective kind + which scaffold to emit.
    let (kind, scaffold) = if let Some(target) = review_target {
        (
            "review".to_string(),
            REVIEW_SCAFFOLD.to_string().replace("{target}", target),
        )
    } else {
        if !is_known_intent_kind(kind) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown intent kind '{}' (expected one of {:?})",
                    kind, INTENT_KIND
                ),
            });
        }
        (kind.to_string(), FEATURE_SCAFFOLD.to_string())
    };

    // A non-default kind is threaded into `IntentCreateOptions` so create writes
    // the `kind:` frontmatter key; the default `feature` stays `None` to keep an
    // ordinary intent's frontmatter (and canonical hash) byte-for-byte unchanged.
    let kind_opt = if kind == "feature" {
        None
    } else {
        Some(kind.clone())
    };

    // Create through the EXISTING vault write path so the intent enters redb
    // normally and joins the merkle exactly as `atomic vault intent create` does
    // — we do NOT invent a new redb write.
    let created = repo
        .vault_intent_create(IntentCreateOptions {
            title: title.to_string(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: kind_opt,
        })
        .map_err(CliError::Repository)?;

    // Overwrite the (legacy positional) scaffold body with the directive
    // scaffold, again through the existing update path. `force` is set because
    // this runs immediately after create; the intent is a fresh backlog draft
    // with no linked goal, so force is a no-op guard-skip.
    //
    // Child ids (acceptance criteria, tasks) are namespaced under the intent's
    // ULID — not the human key — so they are globally unique and never carry the
    // human key's `::`/`-` separators. The frontmatter `kind:` key written at
    // create time survives this body-only update.
    let scaffold = scaffold.replace("{id}", &created.uid);
    repo.vault_intent_update(
        &created.id,
        IntentUpdateOptions {
            content: Some(scaffold),
            force: true,
            ..Default::default()
        },
    )
    .map_err(CliError::Repository)?;

    Ok((created, kind))
}

impl Command for IntentNew {
    fn run(&self) -> CliResult<()> {
        if self.template != "feature" {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown template '{}' (only 'feature' is implemented)",
                    self.template
                ),
            });
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let (created, kind) =
            create_intent(&repo, &self.title, &self.kind, self.review.as_deref())?;

        println!("Created intent: {}", created.id);
        println!("  file: .vault/{}", created.intent_file);
        println!("  template: {}", self.template);
        println!("  kind: {kind}");
        if let Some(target) = &self.review {
            println!("  reviews: {target}");
        }
        println!();
        println!("Edit the directive stubs, then:");
        println!("  atomic intent validate {}", created.id);
        println!("  atomic intent attest {}", created.id);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::{lift_and_attest, validate_intent};
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use tempfile::tempdir;

    use crate::commands::intent::bridge;

    /// A fresh repository with an initialized vault.
    fn init_repo() -> (Repository, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        (repo, dir)
    }

    /// The classification mirrored into the manifest `IntentSummary.kind` — the
    /// exact field `intent list`/`show` read without lifting.
    fn manifest_kind(repo: &Repository, id: &str) -> String {
        repo.vault_manifest()
            .unwrap()
            .intents
            .get(id)
            .unwrap()
            .kind
            .clone()
    }

    #[test]
    fn new_defaults_to_feature() {
        let (repo, _dir) = init_repo();
        let (created, kind) = create_intent(&repo, "Plain work", "feature", None).unwrap();
        assert_eq!(kind, "feature");
        assert_eq!(manifest_kind(&repo, &created.id), "feature");
        // Hash back-compat: a default-kind intent omits the `kind` frontmatter key.
        let inputs = bridge::read_intent(&repo, &created.id).unwrap();
        assert!(!inputs.frontmatter.contains_key("kind"));
    }

    #[test]
    fn new_kind_bug_sets_bug() {
        let (repo, _dir) = init_repo();
        let (created, kind) = create_intent(&repo, "A bug", "bug", None).unwrap();
        assert_eq!(kind, "bug");
        assert_eq!(manifest_kind(&repo, &created.id), "bug");
        let inputs = bridge::read_intent(&repo, &created.id).unwrap();
        assert_eq!(
            inputs.frontmatter.get("kind").and_then(|v| v.as_str()),
            Some("bug")
        );
    }

    #[test]
    fn new_kind_bogus_errors() {
        let (repo, _dir) = init_repo();
        let err = create_intent(&repo, "Nope", "bogus", None).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidArgument { .. }),
            "unknown kind must be a clean argument error, got {err:?}"
        );
    }

    #[test]
    fn new_review_sets_kind_scaffolds_ref_and_gates() {
        let (repo, _dir) = init_repo();
        let target = "urn:atomic:intent:REVIEWED-1";
        // `--review` wins over an explicit `--kind` (feature default here).
        let (created, kind) = create_intent(&repo, "Review it", "feature", Some(target)).unwrap();
        assert_eq!(kind, "review");
        assert_eq!(manifest_kind(&repo, &created.id), "review");

        // The stored body carries the reviews ref naming the target.
        let inputs = bridge::read_intent(&repo, &created.id).unwrap();
        assert!(
            inputs.body.contains("edge=reviews"),
            "review body must carry a reviews ref, got:\n{}",
            inputs.body
        );
        assert!(
            inputs.body.contains(target),
            "review body must name the target, got:\n{}",
            inputs.body
        );

        // It lifts to a review node with a matching reviews edge.
        let node = bridge::lift(&inputs).unwrap();
        assert_eq!(node.kind, "review");
        assert!(
            node.depends_on
                .iter()
                .any(|r| r.edge == "reviews" && r.to == target),
            "lifted node must carry a reviews edge to the target, got {:?}",
            node.depends_on
        );

        // And it passes the gate once attested (ReviewShape satisfied).
        let kp = KeyPair::generate();
        let identity = Identity::new("reviewer", &kp);
        let attested = lift_and_attest(&inputs.frontmatter, &inputs.body, &identity, &kp).unwrap();
        let report = validate_intent(&attested);
        assert!(
            report.conforms,
            "a --review intent must conform to the gate, violations: {:?}",
            report.results
        );
    }

    #[test]
    fn non_review_intent_carries_no_reviews_edge() {
        // The default scaffold must NOT carry a reviews edge — the gate rejects a
        // non-review intent that does (ReviewShape's contrapositive).
        let (repo, _dir) = init_repo();
        let (created, _kind) = create_intent(&repo, "Feature", "feature", None).unwrap();
        let inputs = bridge::read_intent(&repo, &created.id).unwrap();
        let node = bridge::lift(&inputs).unwrap();
        assert!(node.depends_on.iter().all(|r| r.edge != "reviews"));
    }
}
