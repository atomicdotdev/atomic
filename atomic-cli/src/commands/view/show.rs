//! Display the ordered and order-invariant identity of one view.

use clap::Parser;

use atomic_core::types::Base32;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{hint, view as style_view};

/// Show one view's identity and metadata.
#[derive(Parser, Debug, Default)]
#[command(name = "show")]
pub struct Show {
    /// View to inspect. Defaults to the current view.
    pub name: Option<String>,
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(|error| match error {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;
        let name = match &self.name {
            Some(name) => name.clone(),
            None => {
                let working_copy = repo
                    .require_working_copy_id()
                    .map_err(CliError::Repository)?;
                repo.desired_view_name(working_copy)
                    .map_err(CliError::Repository)?
            }
        };
        let info = repo.get_view_info(&name).map_err(CliError::Repository)?;
        let identity = repo
            .refresh_view_set_id_index(&name)
            .map_err(CliError::Repository)?;

        println!("View: {}", style_view(&info.name));
        println!("Scope: {}", info.kind_label());
        println!("Parent: {}", hint(info.parent_display()));
        println!("Changes: {}", identity.closure_len);
        println!("Merkle: {}", identity.merkle.to_base32());
        println!("SetId: {}", identity.set_id.to_base32());
        Ok(())
    }
}
