//! `atomic intent new <KEY> [--template feature]` — scaffold a directive-based
//! intent into the vault.

use clap::Parser;

use atomic_repository::{IntentCreateOptions, IntentUpdateOptions, Repository};

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

:::task{#{id}-1 status=unmet criteria={id}-ac-1}
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

        // Create through the EXISTING vault write path so the intent enters
        // redb normally and joins the merkle exactly as `atomic vault intent
        // create` does — we do NOT invent a new redb write.
        let created = repo
            .vault_intent_create(IntentCreateOptions {
                title: self.title.clone(),
                priority: None,
                assignee: None,
                labels: Vec::new(),
                session_id: None,
                turn_id: None,
            })
            .map_err(CliError::Repository)?;

        // Overwrite the (legacy positional) scaffold body with the directive
        // scaffold, again through the existing update path. `force` is set
        // because this runs immediately after create; the intent is a fresh
        // backlog draft with no linked goal, so force is a no-op guard-skip.
        //
        // Child ids (acceptance criteria, tasks) are namespaced under the
        // intent's ULID — not the human key — so they are globally unique and
        // never carry the human key's `::`/`-` separators.
        let scaffold = FEATURE_SCAFFOLD.replace("{id}", &created.uid.to_lowercase());
        repo.vault_intent_update(
            &created.id,
            IntentUpdateOptions {
                content: Some(scaffold),
                force: true,
                ..Default::default()
            },
        )
        .map_err(CliError::Repository)?;

        println!("Created intent: {}", created.id);
        println!("  file: .vault/{}", created.intent_file);
        println!("  template: {}", self.template);
        println!();
        println!("Edit the directive stubs, then:");
        println!("  atomic intent validate {}", created.id);
        println!("  atomic intent attest {}", created.id);

        Ok(())
    }
}
