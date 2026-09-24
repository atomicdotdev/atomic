use std::fmt::Write as _;

use super::model::{
    ActorDto, EffectPlanDto, EffectReceiptDto, EffectTargetDto, EffectValueDto, GitHeadStateDto,
    GitIndexStateDto, GitObjectDto, GitRefObservationDto, GitRefTargetDto, GitStateDto,
    MetadataTargetDto, MetadataTransitionDto, MetadataValueDto, OperationDetailsDto,
    OperationHeadStateDto, OperationLogDto, OperationRelationDto, OperationScopeDto, RepoStateDto,
    WorkingCopyStateDto,
};

pub(super) fn render_log_human(log: &OperationLogDto) -> String {
    let mut output = String::new();
    writeln!(output, "Operation log").unwrap();
    writeln!(output, "Scope: {}", format_scope(&log.scope)).unwrap();
    match &log.head_state {
        OperationHeadStateDto::Empty => {
            writeln!(output, "Head state: empty").unwrap();
        }
        OperationHeadStateDto::Single { head } => {
            writeln!(output, "Head state: single").unwrap();
            writeln!(output, "  HEAD {head}").unwrap();
        }
        OperationHeadStateDto::Diverged { heads } => {
            writeln!(output, "Head state: DIVERGED ({} heads)", heads.len()).unwrap();
            for head in heads {
                writeln!(output, "  HEAD {head}").unwrap();
            }
        }
    }

    writeln!(output, "Operations:").unwrap();
    if log.entries.is_empty() {
        writeln!(output, "  none").unwrap();
        return output;
    }

    for entry in &log.entries {
        let marker = if entry.is_head { "HEAD" } else { "    " };
        writeln!(output, "  {marker} {}", entry.id).unwrap();
        writeln!(output, "    encoding_version: {}", entry.encoding_version).unwrap();
        writeln!(output, "    kind: {}", entry.kind).unwrap();
        writeln!(
            output,
            "    relation: {}",
            format_relation(entry.relation.as_ref())
        )
        .unwrap();
        writeln!(output, "    scope: {}", format_scope(&entry.scope)).unwrap();
        write_string_list(&mut output, 4, "parents", &entry.parents);
        writeln!(output, "    actor: {}", format_actor(&entry.actor)).unwrap();
        write_timestamp(
            &mut output,
            4,
            entry.timestamp_ms,
            entry.timestamp_utc.as_deref(),
        );
        writeln!(output, "    verification: {}", entry.verification).unwrap();
    }
    output
}

pub(super) fn render_details_human(details: &OperationDetailsDto) -> String {
    let mut output = String::new();
    writeln!(output, "Operation {}", details.id).unwrap();
    writeln!(output, "Encoding version: {}", details.encoding_version).unwrap();
    write_string_list(&mut output, 0, "Parents", &details.parents);
    writeln!(output, "Kind: {}", details.kind).unwrap();
    writeln!(
        output,
        "Relation: {}",
        format_relation(details.relation.as_ref())
    )
    .unwrap();
    writeln!(output, "Scope: {}", format_scope(&details.scope)).unwrap();
    writeln!(output, "Actor: {}", format_actor(&details.actor)).unwrap();
    write_timestamp(
        &mut output,
        0,
        details.timestamp_ms,
        details.timestamp_utc.as_deref(),
    );
    writeln!(output, "Verification: {}", details.verification).unwrap();

    writeln!(output, "Head scopes:").unwrap();
    if details.head_scopes.is_empty() {
        writeln!(output, "  none").unwrap();
    } else {
        for scope in &details.head_scopes {
            writeln!(output, "  - {}", format_scope(scope)).unwrap();
        }
    }

    writeln!(output, "Before:").unwrap();
    write_repo_state(&mut output, 2, &details.before);
    writeln!(output, "Delta:").unwrap();
    writeln!(output, "  after:").unwrap();
    write_repo_state(&mut output, 4, &details.delta.after);
    writeln!(output, "  metadata:").unwrap();
    if details.delta.metadata.is_empty() {
        writeln!(output, "    none").unwrap();
    } else {
        for transition in &details.delta.metadata {
            write_metadata_transition(&mut output, 4, transition);
        }
    }
    writeln!(output, "  effects:").unwrap();
    if details.delta.effects.is_empty() {
        writeln!(output, "    none").unwrap();
    } else {
        for effect in &details.delta.effects {
            write_effect(&mut output, 4, effect);
        }
    }

    writeln!(output, "Git observations:").unwrap();
    if details.git_observed.is_empty() {
        writeln!(output, "  none").unwrap();
    } else {
        for observation in &details.git_observed {
            write_git_observation(&mut output, 2, observation);
        }
    }

    write_string_list(&mut output, 0, "Evidence", &details.evidence);

    writeln!(output, "Loss notes:").unwrap();
    if details.loss_notes.is_empty() {
        writeln!(output, "  none").unwrap();
    } else {
        for note in &details.loss_notes {
            writeln!(output, "  - code: {}", note.code).unwrap();
            writeln!(output, "    message: {}", note.message).unwrap();
            write_string_list(&mut output, 4, "evidence", &note.evidence);
        }
    }

    writeln!(output, "Receipts:").unwrap();
    if details.receipts.is_empty() {
        writeln!(output, "  none").unwrap();
    } else {
        for receipt in &details.receipts {
            write_receipt(&mut output, 2, receipt);
        }
    }
    output
}

pub(super) fn to_pretty_json<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(value)
}

fn write_metadata_transition(
    output: &mut String,
    indent: usize,
    transition: &MetadataTransitionDto,
) {
    writeln!(output, "{}- target:", spaces(indent)).unwrap();
    write_metadata_target(output, indent + 4, &transition.target);
    writeln!(output, "{}  expected_old:", spaces(indent)).unwrap();
    write_metadata_value(output, indent + 4, &transition.expected_old);
    writeln!(output, "{}  expected_new:", spaces(indent)).unwrap();
    write_metadata_value(output, indent + 4, &transition.expected_new);
}

fn write_metadata_target(output: &mut String, indent: usize, target: &MetadataTargetDto) {
    match target {
        MetadataTargetDto::ViewChange { view, change } => {
            write_key_value(output, indent, "kind", "view_change");
            write_key_value(output, indent, "view", view);
            write_key_value(output, indent, "change", change);
        }
        MetadataTargetDto::View { name } => {
            write_key_value(output, indent, "kind", "view");
            write_key_value(output, indent, "name", name);
        }
        MetadataTargetDto::Tag { view, name } => {
            write_key_value(output, indent, "kind", "tag");
            write_key_value(output, indent, "view", view);
            write_key_value(output, indent, "name", name);
        }
        MetadataTargetDto::Remote { name } => {
            write_key_value(output, indent, "kind", "remote");
            write_key_value(output, indent, "name", name);
        }
        MetadataTargetDto::RefMapping { view } => {
            write_key_value(output, indent, "kind", "ref_mapping");
            write_key_value(output, indent, "view", view);
        }
        MetadataTargetDto::Capability { id } => {
            write_key_value(output, indent, "kind", "capability");
            write_key_value(output, indent, "id", id);
        }
    }
}

fn write_metadata_value(output: &mut String, indent: usize, value: &MetadataValueDto) {
    match value {
        MetadataValueDto::Absent => write_key_value(output, indent, "kind", "absent"),
        MetadataValueDto::Sequence { sequence } => {
            write_key_value(output, indent, "kind", "sequence");
            write_key_value(output, indent, "sequence", &sequence.to_string());
        }
        MetadataValueDto::Digest { hash } => {
            write_key_value(output, indent, "kind", "digest");
            write_key_value(output, indent, "hash", hash);
        }
        MetadataValueDto::Bytes { hex } => {
            write_key_value(output, indent, "kind", "bytes");
            write_key_value(output, indent, "hex", hex);
        }
    }
}

fn write_effect(output: &mut String, indent: usize, effect: &EffectPlanDto) {
    writeln!(output, "{}- ordinal: {}", spaces(indent), effect.ordinal).unwrap();
    writeln!(output, "{}  target:", spaces(indent)).unwrap();
    write_effect_target(output, indent + 4, &effect.target);
    writeln!(output, "{}  expected_old:", spaces(indent)).unwrap();
    write_effect_value(output, indent + 4, &effect.expected_old);
    writeln!(output, "{}  expected_new:", spaces(indent)).unwrap();
    write_effect_value(output, indent + 4, &effect.expected_new);
}

fn write_effect_target(output: &mut String, indent: usize, target: &EffectTargetDto) {
    match target {
        EffectTargetDto::FilesystemPath { path } => {
            write_key_value(output, indent, "kind", "filesystem_path");
            write_key_value(output, indent, "path", path);
        }
        EffectTargetDto::WorkspacePath { working_copy, path } => {
            write_key_value(output, indent, "kind", "workspace_path");
            write_key_value(output, indent, "working_copy", working_copy);
            write_key_value(output, indent, "path", path);
        }
        EffectTargetDto::ShelfPath {
            working_copy,
            view,
            path,
        } => {
            write_key_value(output, indent, "kind", "shelf_path");
            write_key_value(output, indent, "working_copy", working_copy);
            write_key_value(output, indent, "view", view);
            write_key_value(output, indent, "path", path);
        }
        EffectTargetDto::GitObject { object } => {
            write_key_value(output, indent, "kind", "git_object");
            write_git_object(output, indent, object);
        }
        EffectTargetDto::GitIndex { working_copy } => {
            write_key_value(output, indent, "kind", "git_index");
            write_key_value(output, indent, "working_copy", working_copy);
        }
        EffectTargetDto::GitRef { name } => {
            write_key_value(output, indent, "kind", "git_ref");
            write_key_value(output, indent, "name", name);
        }
        EffectTargetDto::GitHead { working_copy } => {
            write_key_value(output, indent, "kind", "git_head");
            write_key_value(output, indent, "working_copy", working_copy);
        }
        EffectTargetDto::Checkpoint {
            working_copy,
            checkpoint_kind,
        } => {
            write_key_value(output, indent, "kind", "checkpoint");
            write_key_value(output, indent, "working_copy", working_copy);
            write_key_value(output, indent, "checkpoint_kind", checkpoint_kind);
        }
        EffectTargetDto::WorkingCopy { working_copy } => {
            write_key_value(output, indent, "kind", "working_copy");
            write_key_value(output, indent, "working_copy", working_copy);
        }
        EffectTargetDto::Verification {
            working_copy,
            scope,
        } => {
            write_key_value(output, indent, "kind", "verification");
            write_key_value(
                output,
                indent,
                "working_copy",
                working_copy.as_deref().unwrap_or("none"),
            );
            write_key_value(output, indent, "scope", scope);
        }
    }
}

fn write_effect_value(output: &mut String, indent: usize, value: &EffectValueDto) {
    match value {
        EffectValueDto::Absent => write_key_value(output, indent, "kind", "absent"),
        EffectValueDto::Digest { digest_kind, hash } => {
            write_key_value(output, indent, "kind", "digest");
            write_key_value(output, indent, "digest_kind", digest_kind);
            write_key_value(output, indent, "hash", hash);
        }
        EffectValueDto::File { file } => {
            write_key_value(output, indent, "kind", "file");
            write_key_value(output, indent, "file_kind", &file.kind);
            write_key_value(output, indent, "mode", &format!("{:o}", file.mode));
            write_key_value(output, indent, "content", &file.content);
        }
        EffectValueDto::GitObject { object } => {
            write_key_value(output, indent, "kind", "git_object");
            write_git_object(output, indent, object);
        }
        EffectValueDto::GitRef { target } => {
            write_key_value(output, indent, "kind", "git_ref");
            write_git_ref_target(output, indent, target);
        }
        EffectValueDto::GitIndex { index } => {
            write_key_value(output, indent, "kind", "git_index");
            write_git_index(output, indent, index);
        }
        EffectValueDto::WorkingCopy { state } => {
            write_key_value(output, indent, "kind", "working_copy");
            write_working_copy_state(output, indent, state);
        }
        EffectValueDto::Verification { hash } => {
            write_key_value(output, indent, "kind", "verification");
            write_key_value(output, indent, "hash", hash);
        }
    }
}

fn write_receipt(output: &mut String, indent: usize, receipt: &EffectReceiptDto) {
    writeln!(output, "{}- id: {}", spaces(indent), receipt.id).unwrap();
    write_key_value(output, indent + 2, "operation", &receipt.operation);
    write_key_value(
        output,
        indent + 2,
        "effect_ordinal",
        &receipt
            .effect_ordinal
            .map(|ordinal| ordinal.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    write_key_value(output, indent + 2, "attempt", &receipt.attempt.to_string());
    write_key_value(output, indent + 2, "kind", &receipt.kind);
    write_timestamp(
        output,
        indent + 2,
        receipt.timestamp_ms,
        receipt.timestamp_utc.as_deref(),
    );
    writeln!(output, "{}  observed_old:", spaces(indent)).unwrap();
    match &receipt.observed_old {
        Some(value) => write_effect_value(output, indent + 4, value),
        None => writeln!(output, "{}none", spaces(indent + 4)).unwrap(),
    }
    writeln!(output, "{}  observed_new:", spaces(indent)).unwrap();
    match &receipt.observed_new {
        Some(value) => write_effect_value(output, indent + 4, value),
        None => writeln!(output, "{}none", spaces(indent + 4)).unwrap(),
    }
}

fn write_repo_state(output: &mut String, indent: usize, state: &RepoStateDto) {
    match &state.view {
        Some(view) => {
            writeln!(output, "{}view:", spaces(indent)).unwrap();
            write_key_value(output, indent + 2, "name", &view.name);
            write_key_value(output, indent + 2, "state", &view.state);
            write_key_value(
                output,
                indent + 2,
                "set_id",
                view.set_id.as_deref().unwrap_or("none"),
            );
        }
        None => write_key_value(output, indent, "view", "none"),
    }
    match &state.working_copy {
        Some(working_copy) => {
            writeln!(output, "{}working_copy:", spaces(indent)).unwrap();
            write_working_copy_state(output, indent + 2, working_copy);
        }
        None => write_key_value(output, indent, "working_copy", "none"),
    }
    match &state.git {
        Some(git) => {
            writeln!(output, "{}git:", spaces(indent)).unwrap();
            write_git_state(output, indent + 2, git);
        }
        None => write_key_value(output, indent, "git", "none"),
    }
}

fn write_working_copy_state(output: &mut String, indent: usize, state: &WorkingCopyStateDto) {
    write_key_value(output, indent, "id", &state.id);
    write_key_value(
        output,
        indent,
        "location_fingerprint",
        &state.location_fingerprint,
    );
    write_key_value(
        output,
        indent,
        "desired_view",
        &state.desired_view.to_string(),
    );
    write_key_value(output, indent, "desired_state", &state.desired_state);
    write_key_value(
        output,
        indent,
        "materialized_state",
        state.materialized_state.as_deref().unwrap_or("none"),
    );
    write_key_value(
        output,
        indent,
        "materialized_manifest",
        state.materialized_manifest.as_deref().unwrap_or("none"),
    );
}

fn write_git_state(output: &mut String, indent: usize, git: &GitStateDto) {
    writeln!(output, "{}head:", spaces(indent)).unwrap();
    write_git_head(output, indent + 2, &git.head);
    match &git.index {
        Some(index) => {
            writeln!(output, "{}index:", spaces(indent)).unwrap();
            write_git_index(output, indent + 2, index);
        }
        None => write_key_value(output, indent, "index", "none"),
    }
    write_key_value(output, indent, "refs_digest", &git.refs_digest);
}

fn write_git_head(output: &mut String, indent: usize, head: &GitHeadStateDto) {
    match head {
        GitHeadStateDto::Attached { symref, oid } => {
            write_key_value(output, indent, "kind", "attached");
            write_key_value(output, indent, "symref", symref);
            write_git_object(output, indent, oid);
        }
        GitHeadStateDto::Detached { oid } => {
            write_key_value(output, indent, "kind", "detached");
            write_git_object(output, indent, oid);
        }
        GitHeadStateDto::Unborn { symref } => {
            write_key_value(output, indent, "kind", "unborn");
            write_key_value(output, indent, "symref", symref);
        }
        GitHeadStateDto::MissingTarget { symref } => {
            write_key_value(output, indent, "kind", "missing_target");
            write_key_value(output, indent, "symref", symref);
        }
    }
}

fn write_git_index(output: &mut String, indent: usize, index: &GitIndexStateDto) {
    write_key_value(output, indent, "digest", &index.digest);
    match &index.tree {
        Some(tree) => {
            writeln!(output, "{}tree:", spaces(indent)).unwrap();
            write_git_object(output, indent + 2, tree);
        }
        None => write_key_value(output, indent, "tree", "none"),
    }
}

fn write_git_object(output: &mut String, indent: usize, object: &GitObjectDto) {
    write_key_value(output, indent, "algorithm", &object.algorithm);
    write_key_value(output, indent, "oid", &object.oid);
}

fn write_git_observation(output: &mut String, indent: usize, observation: &GitRefObservationDto) {
    writeln!(output, "{}- name: {}", spaces(indent), observation.name).unwrap();
    match &observation.target {
        Some(target) => {
            writeln!(output, "{}  target:", spaces(indent)).unwrap();
            write_git_ref_target(output, indent + 4, target);
        }
        None => writeln!(output, "{}  target: none", spaces(indent)).unwrap(),
    }
}

fn write_git_ref_target(output: &mut String, indent: usize, target: &GitRefTargetDto) {
    match target {
        GitRefTargetDto::Direct { object } => {
            write_key_value(output, indent, "kind", "direct");
            write_git_object(output, indent, object);
        }
        GitRefTargetDto::Symbolic { name } => {
            write_key_value(output, indent, "kind", "symbolic");
            write_key_value(output, indent, "name", name);
        }
    }
}

fn format_relation(relation: Option<&OperationRelationDto>) -> String {
    match relation {
        Some(OperationRelationDto::Undo { target }) => format!("undo target={target}"),
        Some(OperationRelationDto::Restore { target }) => format!("restore target={target}"),
        None => "none".to_string(),
    }
}

fn format_scope(scope: &OperationScopeDto) -> String {
    match scope {
        OperationScopeDto::Repository => "repository".to_string(),
        OperationScopeDto::WorkingCopy { id } => format!("working_copy:{id}"),
    }
}

fn format_actor(actor: &ActorDto) -> String {
    match actor {
        ActorDto::Human { did } => format!("human did={did}"),
        ActorDto::Agent { did, session, turn } => format!(
            "agent did={did} session={session} turn={}",
            turn.map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        ),
        ActorDto::System { name } => format!("system name={name}"),
    }
}

fn write_timestamp(output: &mut String, indent: usize, timestamp_ms: i64, utc: Option<&str>) {
    writeln!(
        output,
        "{}time: {} (timestamp_ms={timestamp_ms})",
        spaces(indent),
        utc.unwrap_or("unrepresentable")
    )
    .unwrap();
}

fn write_string_list(output: &mut String, indent: usize, label: &str, values: &[String]) {
    writeln!(output, "{}{}:", spaces(indent), label).unwrap();
    if values.is_empty() {
        writeln!(output, "{}none", spaces(indent + 2)).unwrap();
    } else {
        for value in values {
            writeln!(output, "{}- {value}", spaces(indent + 2)).unwrap();
        }
    }
}

fn write_key_value(output: &mut String, indent: usize, key: &str, value: &str) {
    writeln!(output, "{}{key}: {value}", spaces(indent)).unwrap();
}

fn spaces(count: usize) -> String {
    " ".repeat(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::op::model::{OperationLossNoteDto, RepoStateDeltaDto};

    #[test]
    fn operation_log_output_marks_all_diverged_heads() {
        let first = "A".repeat(52);
        let second = "B".repeat(52);
        let log = OperationLogDto {
            scope: OperationScopeDto::Repository,
            head_state: OperationHeadStateDto::Diverged {
                heads: vec![first.clone(), second.clone()],
            },
            entries: vec![],
        };

        let output = render_log_human(&log);
        assert!(output.contains("Head state: DIVERGED (2 heads)"));
        assert!(output.contains(&format!("HEAD {first}")));
        assert!(output.contains(&format!("HEAD {second}")));
    }

    #[test]
    fn operation_show_output_names_every_required_section_and_effect_lease() {
        let details = OperationDetailsDto {
            id: "C".repeat(52),
            encoding_version: 2,
            parents: vec!["D".repeat(52)],
            kind: "switch_view".to_string(),
            relation: None,
            scope: OperationScopeDto::WorkingCopy {
                id: "01TESTWORKINGCOPY0000000000".to_string(),
            },
            actor: ActorDto::System {
                name: "test".to_string(),
            },
            timestamp_ms: 0,
            timestamp_utc: Some("1970-01-01T00:00:00.000Z".to_string()),
            verification: "verified".to_string(),
            before: RepoStateDto {
                view: None,
                working_copy: None,
                git: None,
            },
            delta: RepoStateDeltaDto {
                after: RepoStateDto {
                    view: None,
                    working_copy: None,
                    git: None,
                },
                metadata: vec![],
                effects: vec![EffectPlanDto {
                    ordinal: 0,
                    target: EffectTargetDto::FilesystemPath {
                        path: "src/main.rs".to_string(),
                    },
                    expected_old: EffectValueDto::Absent,
                    expected_new: EffectValueDto::Verification {
                        hash: "E".repeat(52),
                    },
                }],
            },
            git_observed: vec![],
            evidence: vec!["F".repeat(52)],
            loss_notes: vec![OperationLossNoteDto {
                code: "test_loss".to_string(),
                message: "test note".to_string(),
                evidence: vec!["G".repeat(52)],
            }],
            receipts: vec![],
            head_scopes: vec![OperationScopeDto::Repository],
        };

        let output = render_details_human(&details);
        for required in [
            "Operation ",
            "Parents:",
            "Kind: switch_view",
            "Scope:",
            "Actor:",
            "time:",
            "Verification: verified",
            "Head scopes:",
            "Before:",
            "Delta:",
            "effects:",
            "expected_old:",
            "expected_new:",
            "Git observations:",
            "Evidence:",
            "Loss notes:",
            "Receipts:",
        ] {
            assert!(output.contains(required), "missing {required:?}:\n{output}");
        }
    }
}
