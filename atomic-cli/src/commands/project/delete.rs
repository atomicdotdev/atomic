//! The `project delete` command for deleting a remote project.
//!
//! # Usage
//!
//! ```text
//! atomic project delete <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Project path in 'workspace/project' format
//!
//! Options:
//!       --org <ORG>  Organization override
//!   -f, --force      Skip confirmation prompt
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Delete a project (with confirmation prompt)
//! $ atomic project delete my-workspace/old-project
//! ? Delete project 'old-project' from workspace 'my-workspace'? This cannot be undone. yes
//! ✓ Deleted project old-project from workspace my-workspace
//!
//! # Force delete without confirmation
//! $ atomic project delete my-workspace/old-project --force
//! ✓ Deleted project old-project from workspace my-workspace
//! ```

use clap::Parser;

use atomic_remote::StorageClient;

use crate::commands::client::{build_client, remote_err};
use crate::commands::project::parse_project_path;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_error, print_success};

/// Delete a remote project.
///
/// Permanently removes a project and all its data from the server.
/// Requires the project path in `workspace/project` format.
///
/// Without `--force`, prompts for confirmation before deleting.
#[derive(Debug, Parser)]
#[command(name = "delete")]
pub struct ProjectDelete {
    /// Project path in 'workspace/project' format.
    ///
    /// Example: `my-workspace/my-project`
    #[arg(required = true)]
    pub slug: String,

    /// Organization to delete the project from.
    ///
    /// Overrides the default organization from global config.
    #[arg(long)]
    pub org: Option<String>,

    /// Force deletion without confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Command for ProjectDelete {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let (ws_slug, proj_slug) = parse_project_path(&self.slug)?;
            let client = build_client(self.org.as_deref(), None).await?;

            self.execute_with_client(&client, ws_slug, proj_slug, |prompt| {
                dialoguer::Confirm::new()
                    .with_prompt(prompt)
                    .default(false)
                    .interact()
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))
            })
            .await
        })
    }
}

impl ProjectDelete {
    async fn execute_with_client<F>(
        &self,
        client: &StorageClient,
        ws_slug: &str,
        proj_slug: &str,
        confirm: F,
    ) -> CliResult<()>
    where
        F: FnOnce(&str) -> CliResult<bool>,
    {
        // Interactive deletes validate the target before asking the user to
        // confirm. Forced deletes keep the single-request path for scripts.
        if !self.force {
            client
                .get_project(ws_slug, proj_slug)
                .await
                .map_err(|e| map_project_error(e, ws_slug, proj_slug))?;

            let prompt = format!(
                "Delete project '{}' from workspace '{}'? This cannot be undone.",
                proj_slug, ws_slug,
            );

            if !confirm(&prompt)? {
                return Err(CliError::Cancelled);
            }
        }

        client
            .delete_project(ws_slug, proj_slug)
            .await
            .map_err(|e| map_project_error(e, ws_slug, proj_slug))?;

        print_success(&format!(
            "Deleted project {} from workspace {}",
            proj_slug, ws_slug,
        ));

        Ok(())
    }
}

fn map_project_error(
    error: atomic_remote::RemoteError,
    ws_slug: &str,
    proj_slug: &str,
) -> CliError {
    if error.is_not_found() {
        print_error(&format!(
            "Project '{}' not found in workspace '{}'",
            proj_slug, ws_slug,
        ));
    }

    remote_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    #[test]
    fn fields_default() {
        let cmd = ProjectDelete {
            slug: "ws/proj".to_string(),
            org: None,
            force: false,
        };
        assert_eq!(cmd.slug, "ws/proj");
        assert!(!cmd.force);
        assert!(cmd.org.is_none());
    }

    #[test]
    fn fields_with_force_and_org() {
        let cmd = ProjectDelete {
            slug: "team/backend".to_string(),
            org: Some("acme".to_string()),
            force: true,
        };
        assert_eq!(cmd.slug, "team/backend");
        assert!(cmd.force);
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[tokio::test]
    async fn missing_project_is_rejected_before_confirmation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..bytes_read]);
            assert!(
                request.starts_with("GET /workspaces/ws/projects/missing "),
                "unexpected request: {request}"
            );

            let body =
                r#"{"success":false,"error":{"code":"NOT_FOUND","message":"project not found"}}"#;
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let client =
            StorageClient::new(&format!("http://{address}"), "acme", "test-token").unwrap();
        let cmd = ProjectDelete {
            slug: "ws/missing".to_string(),
            org: None,
            force: false,
        };
        let prompt_called = AtomicBool::new(false);

        let result = cmd
            .execute_with_client(&client, "ws", "missing", |_| {
                prompt_called.store(true, Ordering::SeqCst);
                Ok(true)
            })
            .await;

        server.join().unwrap();
        assert!(result.is_err());
        assert!(
            !prompt_called.load(Ordering::SeqCst),
            "confirmation was requested before project existence was checked"
        );
    }
}
