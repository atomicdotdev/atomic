#!/usr/bin/env python3
"""Independent reviewer: verify an Atomic DSSE export and emit its exact payload.

Requires cryptography. The public key must come from an independent trust
decision, not the envelope's keyid. Native proofs and graph claims are unchecked.
"""

import argparse
import base64
import json
import sys

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

PAYLOAD_TYPE = "application/vnd.atomic.provenance-export.v1+json"
MAX_BYTES = 8 * 1024 * 1024


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON member")
        result[key] = value
    return result


def refuse_number(_):
    raise ValueError("only I-JSON integers are admitted")


def integer(value):
    parsed = int(value)
    if abs(parsed) > 9007199254740991:
        raise ValueError("integer exceeds I-JSON exact range")
    return parsed


def admitted_json(raw):
    if len(raw) > MAX_BYTES:
        raise ValueError("export exceeds 8 MiB limit")
    value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object, parse_int=integer,
                       parse_float=refuse_number, parse_constant=refuse_number)
    def validate(item, depth=0):
        if isinstance(item, (dict, list)):
            if depth >= 128:
                raise ValueError("JSON exceeds nesting limit")
            if isinstance(item, dict):
                for key, child in item.items():
                    key.encode("utf-8", errors="strict")
                    validate(child, depth + 1)
            else:
                for child in item:
                    validate(child, depth + 1)
        elif isinstance(item, str):
            item.encode("utf-8", errors="strict")
    validate(value)
    return value


def decode(value):
    if not isinstance(value, str):
        raise ValueError("base64 member must be a string")
    for altchars in (None, b"-_"):
        try:
            decoded = base64.b64decode(value, altchars=altchars, validate=True)
            encoded = base64.b64encode(decoded, altchars=altchars).decode("ascii")
            if value == encoded:
                return decoded
        except (ValueError, UnicodeError):
            pass
    raise ValueError("noncanonical base64")


def verify(raw, key_bytes):
    envelope = admitted_json(raw)
    if envelope["payloadType"] != PAYLOAD_TYPE:
        raise ValueError("wrong payload type")
    payload = decode(envelope["payload"])
    kind = PAYLOAD_TYPE.encode("utf-8")
    pae = b"DSSEv1 " + str(len(kind)).encode() + b" " + kind
    pae += b" " + str(len(payload)).encode() + b" " + payload
    key = Ed25519PublicKey.from_public_bytes(key_bytes)
    signatures = envelope["signatures"]
    if not isinstance(signatures, list) or not 1 <= len(signatures) <= 1024:
        raise ValueError("invalid signature count")
    accepted = False
    for entry in signatures:
        try:
            key.verify(decode(entry["sig"]), pae)
            accepted = True
            break
        except (ValueError, TypeError, KeyError, InvalidSignature):
            continue
    if not accepted:
        raise ValueError("no signature verifies under pinned key")
    # Interpret only the same payload bytes used to construct the PAE above.
    value = admitted_json(payload)
    if set(value) != {"schema", "coverage", "projections"}:
        raise ValueError("wrong export fields")
    if value["schema"] != "atomic-provenance-export/v1":
        raise ValueError("wrong export schema")
    if value["coverage"] != "exporter-signed-projections-only":
        raise ValueError("unsupported coverage claim")
    projections = value["projections"]
    if not isinstance(projections, list) or not projections:
        raise ValueError("no projections")
    for projection in projections:
        if not isinstance(projection.get("@id"), str) or not projection["@id"].startswith("urn:atomic:provgraph:"):
            raise ValueError("not an Atomic provenance projection")
        if not isinstance(projection.get("@graph"), list):
            raise ValueError("missing graph")
    return payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", help="Envelope JSON file, or - for stdin")
    parser.add_argument("--public-key", required=True, help="Independently pinned base32 Ed25519 key")
    args = parser.parse_args()
    try:
        if args.file == "-":
            raw = sys.stdin.buffer.read(MAX_BYTES + 1)
        else:
            with open(args.file, "rb") as stream:
                raw = stream.read(MAX_BYTES + 1)
        key = base64.b32decode(args.public_key + "=" * (-len(args.public_key) % 8))
        payload = verify(raw, key)
    except Exception as error:
        print(f"refused: {error}", file=sys.stderr)
        return 1
    sys.stdout.buffer.write(payload)
    return 0


if __name__ == "__main__":
    sys.exit(main())
