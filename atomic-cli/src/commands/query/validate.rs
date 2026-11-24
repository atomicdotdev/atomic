use super::*;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    #[default]
    Projection,
    Publication,
    Review,
    /// Validate user-defined vault kinds against their generated shapes.
    Kind,
}
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum Format {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Parser)]
pub struct Validate {
    #[arg(long, value_enum, default_value = "projection")]
    pub profile: Profile,
    #[arg(long)]
    pub shape: Option<String>,
    #[arg(long)]
    pub node: Option<String>,
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

impl Command for Validate {
    fn run(&self) -> CliResult<()> {
        // Validation deliberately runs in the CLI process so neither the
        // daemon nor shared libraries acquire the SHACL engine's MSRV.
        let root = find_repository_root()?;
        let snapshot = crate::commands::rpc::workflow_snapshot(&root)?;
        let dot_dir = Repository::canonical_dot_dir(&root).map_err(CliError::Repository)?;
        let error = |error: atomic_shacl::ValidationError| CliError::ShaclError(error.to_string());

        // The kind profile is generated from the recorded kind registry, not
        // from `.atomic/shapes`. It shares the gate with `kind validate`.
        if self.profile == Profile::Kind {
            let repo = crate::commands::vault::kind::open_for_read()?;
            let shapes = crate::commands::vault::kind::shapes::kind_shapes(
                &repo,
                &atomic_repository::KindSource::WorkingCopy,
                None,
            )?;
            if shapes.bundles.is_empty() {
                return Err(CliError::InvalidArgument {
                    message: "no registered kinds to validate".into(),
                });
            }
            let dataset = crate::commands::vault::kind::shapes::kind_dataset(&repo, &shapes)?;
            let request = atomic_shacl::ValidationRequest {
                profile: atomic_shacl::Profile::Kind,
                shape: self.shape.clone(),
                node: self.node.clone(),
                custom: shapes.bundles,
            };
            let mut report = atomic_shacl::validate_ntriples(
                &dataset.ntriples,
                &dataset.snapshot_identity,
                &request,
            )
            .map_err(error)?;
            if !dataset.missing.is_empty() {
                report.conforms = false;
                report.diagnostics.push(format!(
                    "{} kind instance(s) have no projected knowledge-graph node: {}",
                    dataset.missing.len(),
                    dataset.missing.join(", ")
                ));
            }
            return self.print(report);
        }

        let data =
            atomic_shacl::snapshot_ntriples(&snapshot.nodes, &snapshot.edges).map_err(error)?;
        let request = atomic_shacl::ValidationRequest {
            profile: match self.profile {
                Profile::Projection => atomic_shacl::Profile::Projection,
                Profile::Publication => atomic_shacl::Profile::Publication,
                Profile::Review => atomic_shacl::Profile::Review,
                Profile::Kind => unreachable!("handled above"),
            },
            shape: self.shape.clone(),
            node: self.node.clone(),
            custom: atomic_shacl::load_custom_shapes(&dot_dir.join("shapes")).map_err(error)?,
        };
        let report =
            atomic_shacl::validate_ntriples(&data, &snapshot.identity, &request).map_err(error)?;
        self.print(report)
    }
}

impl Validate {
    fn print(&self, report: atomic_shacl::ValidationOutput) -> CliResult<()> {
        match self.format {
            Format::Json => println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|error| CliError::Internal(error.into()))?
            ),
            Format::Text => {
                println!(
                    "SHACL {:?}: {}",
                    report.profile,
                    if report.conforms {
                        "conforms"
                    } else {
                        "violations"
                    }
                );
                println!(
                    "Snapshot: {}\nProfile: {}",
                    report.snapshot_identity, report.profile_identity
                );
                for coverage in &report.target_coverage {
                    println!(
                        "  {}: {} evaluated / {} applicable",
                        coverage.shape, coverage.evaluated, coverage.applicable
                    );
                }
                for result in &report.results {
                    println!(
                        "  {} {}: {} (path {}, shape {}, component {})",
                        result["result_severity"],
                        result["focus_node"],
                        result["result_messages"],
                        result["result_path"],
                        result["source_shape"],
                        result["source_constraint_component"]
                    );
                }
                for diagnostic in &report.diagnostics {
                    println!("  {diagnostic}");
                }
            }
        }
        if report.conforms {
            Ok(())
        } else {
            Err(CliError::ShaclViolation {
                count: report.results.len(),
            })
        }
    }
}
