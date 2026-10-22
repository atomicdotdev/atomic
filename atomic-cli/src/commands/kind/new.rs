//! `atomic kind new <k>` — create an instance of a registered kind.
//!
//! The instance is validated against the kind's spec (field types, enum
//! values, required fields, refs that must resolve) before anything is
//! written; on any violation nothing is written and the command exits 2.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;

use clap::Parser;
use serde_json::{Map, Value};

use atomic_canonical::kind_spec::{apply_defaults, instance_kg_id, parse_set_value};
use atomic_repository::{
    check_instance, kind_definition_path, kind_instance_path, mint_kind_uid,
    render_frontmatter_document, InstanceCheck, KindRefResolver, KindSource, KindStatus,
};

use super::init::{ingest, write_new_file};
use super::{display_path, kind_not_found, open_repository, record_hint, validation_failed};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{error, success, warning};

/// Create an instance of a registered kind.
///
/// Field values are checked against the kind's definition before the file
/// is written; on any violation nothing is written. The file is ingested
/// into the vault index but not recorded.
#[derive(Parser, Debug)]
#[command(name = "new")]
pub struct KindNew {
    /// The kind to create an instance of.
    pub kind: String,

    /// The instance title.
    #[arg(long)]
    pub title: Option<String>,

    /// Set a declared field: `--set status=accepted`. List fields take a
    /// comma-separated value. Repeat for each field; unset fields get their
    /// declared default.
    #[arg(long = "set", value_name = "FIELD=VALUE")]
    pub set: Vec<String>,

    /// Read the instance body from a file.
    #[arg(long, value_name = "FILE", conflicts_with = "body_stdin")]
    pub body_file: Option<PathBuf>,

    /// Read the instance body from stdin.
    #[arg(long)]
    pub body_stdin: bool,

    /// Emit `{"uid","kind","file","kg_id"}` instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

/// One reason the instance cannot be written.
#[derive(Debug, serde::Serialize)]
struct Rejection {
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    message: String,
}

impl KindNew {
    fn read_body(&self) -> CliResult<String> {
        let mut body = if let Some(path) = &self.body_file {
            std::fs::read_to_string(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => CliError::FileNotFound { path: path.clone() },
                _ => CliError::Io(e),
            })?
        } else if self.body_stdin {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            String::new()
        };
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        Ok(body)
    }

    /// Print why nothing was written and return the exit-2 error.
    fn reject(&self, rejections: &[Rejection]) -> CliError {
        if self.json {
            let out = serde_json::json!({
                "kind": self.kind,
                "written": false,
                "violations": rejections,
            });
            println!("{out}");
        } else {
            for r in rejections {
                match &r.field {
                    Some(field) => eprintln!("{} {field}: {}", error("✗"), r.message),
                    None => eprintln!("{} {}", error("✗"), r.message),
                }
            }
            eprintln!("Nothing was written.");
        }
        validation_failed(format!(
            "the new {} instance does not conform ({} problem(s))",
            self.kind,
            rejections.len()
        ))
    }
}

impl Command for KindNew {
    fn run(&self) -> CliResult<()> {
        let repo = open_repository()?;
        let registry = repo
            .kind_registry(&KindSource::WorkingCopy)
            .map_err(CliError::Repository)?;
        let entry = registry
            .get(&self.kind)
            .ok_or_else(|| kind_not_found(&self.kind))?;
        let spec = match &entry.definition.status {
            KindStatus::Registered(spec) => spec,
            KindStatus::Invalid(problems) => {
                let def = display_path(&entry.definition.def_path);
                for p in problems {
                    eprintln!("{} {def}: {p}", error("✗"));
                }
                return Err(validation_failed(format!(
                    "kind '{}' is not registered until {def} is fixed; nothing was written",
                    self.kind
                )));
            }
        };

        // Coerce every --set before touching anything.
        let mut fm = Map::new();
        let mut rejections = Vec::new();
        let mut seen = BTreeSet::new();
        for raw in &self.set {
            let Some((name, value)) = raw.split_once('=') else {
                return Err(CliError::InvalidArgument {
                    message: format!("--set {raw:?}: expected FIELD=VALUE"),
                });
            };
            let name = name.trim();
            if !seen.insert(name.to_string()) {
                return Err(CliError::InvalidArgument {
                    message: format!("--set {name} given more than once"),
                });
            }
            let Some(field) = spec.field(name) else {
                let message = match name {
                    "title" => "set the title with --title".to_string(),
                    "uid" => "the uid is minted by `atomic kind new`".to_string(),
                    _ => format!(
                        "not a field of {} (declared: {})",
                        self.kind,
                        declared_fields(spec)
                    ),
                };
                rejections.push(Rejection {
                    field: Some(name.to_string()),
                    message,
                });
                continue;
            };
            match parse_set_value(field, value) {
                Ok(v) => {
                    fm.insert(name.to_string(), v);
                }
                Err(e) => rejections.push(Rejection {
                    field: Some(name.to_string()),
                    message: e.to_string(),
                }),
            }
        }
        if !rejections.is_empty() {
            return Err(self.reject(&rejections));
        }

        apply_defaults(spec, &mut fm);
        let uid = mint_kind_uid();
        fm.insert("uid".into(), Value::String(uid.clone()));
        if let Some(title) = &self.title {
            fm.insert("title".into(), Value::String(title.clone()));
        }
        let body = self.read_body()?;
        let document = render_frontmatter_document(&fm, &body);

        // Validate the exact bytes that would be written (fail closed).
        let files = repo
            .kind_files(&KindSource::WorkingCopy)
            .map_err(CliError::Repository)?;
        let resolver = KindRefResolver::new(&files);
        match check_instance(spec, &uid, document.as_bytes(), &resolver) {
            InstanceCheck::Checked { report, .. } if report.conforms => {}
            InstanceCheck::Checked { report, .. } => {
                let rejections: Vec<Rejection> = report
                    .violations
                    .into_iter()
                    .map(|v| Rejection {
                        field: v.field,
                        message: v.message,
                    })
                    .collect();
                return Err(self.reject(&rejections));
            }
            InstanceCheck::Problem { message, .. } => {
                return Err(self.reject(&[Rejection {
                    field: None,
                    message,
                }]));
            }
        }

        let rel = kind_instance_path(&self.kind, &uid);
        let path = repo.vault_dir().join(&rel);
        write_new_file(&path, &document)?;
        ingest(&repo, &rel, &document)?;

        warn_if_definition_unrecorded(&repo, &self.kind)?;

        let kg_id = instance_kg_id(&self.kind, &uid);
        if self.json {
            let out = serde_json::json!({
                "uid": uid,
                "kind": self.kind,
                "file": display_path(&rel),
                "kg_id": kg_id,
            });
            println!("{out}");
        } else {
            println!("{} {}  ({kg_id})", success("Created"), display_path(&rel));
        }
        Ok(())
    }
}

/// The definition must be recorded in the current view before instances
/// are, or the instances' changes will not depend on it (and an `insert`
/// of an instance into another view would not bring its schema along).
fn warn_if_definition_unrecorded(
    repo: &atomic_repository::Repository,
    kind: &str,
) -> CliResult<()> {
    let def = display_path(&kind_definition_path(kind));
    let view = repo.current_view();
    let recorded = repo
        .visible_file_paths(view)
        .map_err(CliError::Repository)?
        .contains(&def);
    if !recorded {
        eprintln!(
            "{} {def} is not recorded in view '{view}' yet, so changes to this instance won't \
             depend on it. Record the definition first:",
            warning("⚠")
        );
        for line in record_hint(
            &kind_definition_path(kind),
            &format!("feat: add {kind} kind"),
        ) {
            eprintln!("  {line}");
        }
    }
    Ok(())
}

fn declared_fields(spec: &atomic_canonical::kind_spec::KindSpec) -> String {
    if spec.fields.is_empty() {
        "none".to_string()
    } else {
        spec.fields.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}
