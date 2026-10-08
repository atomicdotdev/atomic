#!/usr/bin/env python3
"""Compile the draft and check its security-relevant descriptor invariants.

Run with Python from a throwaway venv containing grpcio-tools. Generated modules
and descriptors live only in a temporary directory, never in the source tree.
This checks the wire contract, not runtime authorization or transaction behavior.
"""

import argparse
import importlib
from pathlib import Path
import sys
import tempfile

from google.protobuf import descriptor_pb2
import grpc_tools
from grpc_tools import protoc


SANDBOX_METHODS = {
    "DaemonService": {"Health", "GetCapabilities"},
    "SandboxService": {
        "Materialize", "GetFileStates", "GetChanges", "SubmitChange", "PublishProvenance",
    },
    "ProvenanceService": {
        "ReserveTurn", "AppendEnvelopes", "PrepareCheckpoint", "LoadFrozenEnvelopes",
        "BindCheckpointHash", "AcknowledgeCheckpoint", "UpdateTurn", "GetTurn",
        "GetSession", "GetProvenance", "ExportProvenance",
    },
    "VaultService": {
        "InitVault", "GetVaultEntry", "ListVaultEntries", "GetVaultContext",
        "CreateVaultEntity", "UpdateVaultEntity", "DeleteVaultEntity",
        "ValidateVaultEntity", "LinkVaultEntities",
    },
    "AttestationService": {
        "PrepareAttestation", "RecordAttestation", "VerifyAttestation", "ListAttestations",
    },
    "KnowledgeService": {"QueryGraph"},
}

RETIRED_FIELDS = {
    "StatusRequest": {2}, "ResolveRepositoryResponse": {2},
    "MaterializeRequest": {2, 3}, "GetFileStatesRequest": {2, 3},
    "GetChangesRequest": {2, 3}, "SubmitChangeRequest": {3, 4, 5},
    "PublishProvenanceRequest": {3, 4, 6}, "DiffRequest": {2},
    "RestoreRequest": {5}, "InsertChangesRequest": {3, 8, 10, 11},
    "InsertChangesResponse": {2}, "UnrecordRequest": {3},
    "UnrecordResponse": {2}, "ApplyStashRequest": {4},
    "CreateViewRequest": {5, 7}, "ImportFromGitRequest": {7},
    "ImportFromGitResponse": {3}, "ExportProvenanceRequest": {2, 4},
    "ExportProvenanceResponse": {2},
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--temp-dir", help="Existing parent for temporary compiler output")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    files = sorted(p.relative_to(root).as_posix() for p in (root / "atomic").rglob("*.proto"))
    failures = []

    def check(condition, description):
        if not condition:
            failures.append(description)

    with tempfile.TemporaryDirectory(prefix="atomic-proto-", dir=args.temp_dir) as output:
        descriptor_path = Path(output) / "atomic.pb"
        result = protoc.main([
            "protoc", f"-I{root}",
            f"-I{Path(grpc_tools.__file__).parent / '_proto'}",
            f"--descriptor_set_out={descriptor_path}", "--include_imports",
            f"--python_out={output}", *files,
        ])
        if result:
            return result
        sys.path.insert(0, output)
        options = importlib.import_module("atomic.common.options_pb2")
        common = importlib.import_module("atomic.common.common_pb2")
        check(common.CONTRACT_VERSION_2 == 2, "Contract revision 2 constant changed")
        descriptors = descriptor_pb2.FileDescriptorSet.FromString(descriptor_path.read_bytes())
        messages = {
            f".atomic.{message.name}": message
            for file in descriptors.file if file.package == "atomic"
            for message in file.message_type
        }
        methods = {
            (service.name, method.name): method
            for file in descriptors.file if file.package == "atomic"
            for service in file.service for method in service.method
        }
        writes = {
            options.RPC_EFFECT_REPOSITORY_WRITE,
            options.RPC_EFFECT_WORKSPACE_WRITE,
            options.RPC_EFFECT_DAEMON_WRITE,
        }
        workspace = {options.RPC_EFFECT_WORKSPACE_READ, options.RPC_EFFECT_WORKSPACE_WRITE}
        event_publish = {("ReactorService", "Publish"), ("ReactorService", "PublishStream")}
        negotiation = {("DaemonService", "Health"), ("DaemonService", "GetCapabilities")}
        mutation_permissions = {
            "sandbox.submit", "agent.checkpoint", "view.create", "view.switch",
            "view.delete", "view.promote", "reactor.publish", "reactor.checkpoint",
        }
        observed_sandbox = {}
        for key, method in methods.items():
            label = ".".join(key)
            opt = method.options
            callers = set(opt.Extensions[options.allowed_caller])
            effects = set(opt.Extensions[options.effect])
            capabilities = set(opt.Extensions[options.required_capability])
            scope = opt.Extensions[options.execution_scope]
            check(scope != options.EXECUTION_SCOPE_UNSPECIFIED, f"{label}: missing scope")
            check(callers and options.CALLER_CLASS_UNSPECIFIED not in callers, f"{label}: missing caller allowlist")
            check(effects and options.RPC_EFFECT_UNSPECIFIED not in effects, f"{label}: missing effects")
            check(bool(capabilities) == (key not in negotiation), f"{label}: incorrect negotiation capability exception")
            if scope == options.EXECUTION_SCOPE_LOCAL:
                check(callers == {options.CALLER_CLASS_LOCAL}, f"{label}: LOCAL operation allows network callers")
            if effects & workspace:
                check(scope == options.EXECUTION_SCOPE_LOCAL, f"{label}: workspace effects outside LOCAL")
                check(options.CALLER_CLASS_SANDBOX not in callers, f"{label}: sandbox host filesystem access")
            if not effects & writes:
                check(not any(c.endswith((".write", ".admin")) for c in capabilities), f"{label}: read requires write/admin authority")
                check(not any(f.type_name == ".atomic.ResponseMeta" for f in messages[method.output_type].field), f"{label}: read has mutation replay response")
            elif key not in event_publish:
                check(any(c.endswith((".write", ".admin")) for c in capabilities) or bool(capabilities & mutation_permissions), f"{label}: mutation authorized by read-only capabilities")
                check(any(f.type_name == ".atomic.RequestMeta" for f in messages[method.input_type].field), f"{label}: mutation missing request metadata")
                check(any(f.number == 100 and f.type_name == ".atomic.ResponseMeta" for f in messages[method.output_type].field), f"{label}: mutation missing response echo")
            if options.CALLER_CLASS_SANDBOX in callers:
                observed_sandbox.setdefault(key[0], set()).add(key[1])
                check(not capabilities & {"repository.write", "repository.read", "sandbox.admin", "daemon.admin", "view.create", "view.promote"}, f"{label}: sandbox receives broad authority")
                check(not effects & (workspace | {options.RPC_EFFECT_DAEMON_WRITE}), f"{label}: sandbox mutates host/daemon state")
            if options.RPC_EFFECT_WORKSPACE_WRITE in effects:
                check("working-copy.write" in capabilities, f"{label}: workspace write lacks workspace permission")
        check(observed_sandbox == SANDBOX_METHODS, "Sandbox method allowlist differs from reviewed contract")

        def field(message, name, kind):
            fields = messages[f".atomic.{message}"].field
            check(any(f.name == name and f.type_name == f".atomic.{kind}" for f in fields), f"{message}.{name}: missing {kind}")

        for name, numbers in RETIRED_FIELDS.items():
            msg = messages[f".atomic.{name}"]
            active = {f.number for f in msg.field}
            for number in numbers:
                check(number not in active and any(r.start <= number < r.end for r in msg.reserved_range), f"{name}: retired field {number} not reserved")
        field("SubmitChangeRequest", "expected_snapshot", "ViewSnapshot")
        field("InsertChangesRequest", "expected_snapshot", "ViewSnapshot")
        field("PublishProvenanceRequest", "turn", "SessionTurn")
        field("ResolveRepositoryResponse", "workspace", "WorkspaceInfo")
        field("SandboxOpened", "repository", "RepositoryRef")
        field("SandboxOpened", "target", "ViewRef")
        field("SandboxOpened", "snapshot", "ViewSnapshot")
        field("SandboxSkeleton", "snapshot", "ViewSnapshot")
        field("SandboxSlice", "snapshot", "ViewSnapshot")
        check(any(f.name == "expected_generation" for f in messages[".atomic.UpdateTurnRequest"].field), "UpdateTurn: missing generation fence")
        check(any(f.name == "expected_generation" for f in messages[".atomic.PublishProvenanceRequest"].field), "PublishProvenance: missing generation fence")
        check(any(f.name == "destination_path" for f in messages[".atomic.ImportFromGitRequest"].field), "Git import: missing bootstrap destination")
        for key in methods:
            if key[0] == "VaultService":
                field(messages[methods[key].input_type].name, "view", "ViewRef")
        for name in ["InitVaultRequest", "SyncVaultRequest", "CreateVaultEntityRequest", "DeleteVaultEntityRequest", "LinkVaultEntitiesRequest"]:
            field(name, "expected_snapshot", "ViewSnapshot")
        field("UpdateVaultEntityRequest", "expected", "ExpectedState")
        for service, names in {
            "ViewService": {"CreateView", "DeleteView", "SetViewScope"},
            "RepositoryMutationService": {"InsertChanges", "Unrecord"},
            "ProvenanceService": {"ForkSession"},
            "TriageService": {"ListTriageCandidates", "GenerateTriageReview"},
            "MaintenanceService": {"CheckRepository"},
        }.items():
            for name in names:
                check(methods[(service, name)].options.Extensions[options.execution_scope] == options.EXECUTION_SCOPE_BOTH, f"{service}.{name}: repository-only operation is not BOTH")
        check(options.execution_scope.number == 51001 and options.required_capability.number == 51002, "Original extension numbers changed")
        if failures:
            for failure in failures:
                print(f"FAIL: {failure}", file=sys.stderr)
            return 1
        scopes = {name: sum(m.options.Extensions[options.execution_scope] == value for m in methods.values()) for name, value in [("LOCAL", 1), ("REMOTE", 2), ("BOTH", 3)]}
        print(f"Compiled {len(files)} protobuf files; checked {len(methods)} RPCs across {len(set(s for s, _ in methods))} services.")
        print(f"Scopes: {scopes}; sandbox allowlist: {sum(map(len, observed_sandbox.values()))} methods.")
        print("Caller/effect annotations, read/write capabilities, replay metadata, retired fields, routing and fencing: PASS.")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
