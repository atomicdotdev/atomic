# Optional evidence replay for intent criteria

`atomic intent validate ID` retains its ordinary authoring-gate behavior. A
reviewer can opt into replaying narrowly defined claims with a locally selected
checker. This adds refusals; it never sets `acStatus`, signs an intent, writes the
vault, or grants a review. Python and Probity Verify are optional.

The canonical gate checks declared verification records. Replay checks the
particular retained-evidence claim in a consumer-authored policy. A supported
source-text claim means selected passages appear in the pinned source and record;
it cannot prove that tests passed or that the whole criterion is true. Likewise,
an absence claim needs the adapter's declared coverage. Inspect the returned
`details.scope` and limitations before making a completion decision.

## Try the bounded source-coverage example

These commands use a POSIX shell and an executable Python wrapper with a
shebang. The installed Python checker recipe was exercised on Linux; the native
replay integration tests also passed hosted macOS. Windows native context unit
tests passed, but this example supplies no packaged Windows checker wrapper.
On Windows, select a local executable implementing the protocol below. A checker
that cannot start, fails, or returns an invalid response remains
`not_established`; it cannot make validation pass.

Use a dedicated Python environment for the optional checker. The pinned Verify
revision below implements the four claim adapters listed later in this document:

```sh
python3 -m venv /tmp/atomic-evidence-python
/tmp/atomic-evidence-python/bin/pip install \
  'git+https://github.com/probityai/probity-verify.git@e835ce2bd6a960e7a1cc2fa6522f16d55dce728a'
```

Create a **draft** with one criterion in an Atomic repository:

```sh
atomic intent new 'Preserve selected source passages'
# Use the intent ID printed by that command below.
python3 /path/to/atomic/docs/examples/evidence-replay/prepare-example.py \
  ID /tmp/atomic-evidence-example
PATH="/tmp/atomic-evidence-python/bin:$PATH" atomic intent validate ID --json \
  --replay-evidence /tmp/atomic-evidence-example/manifest.json \
  --evidence-checker /path/to/atomic/docs/examples/evidence-replay/probity-checker.py
```

The synthetic passage claim reports `supported`. An unattested draft still fails
the ordinary gate (exit 2); the example does not make incomplete work complete.
After normal authoring and attestation, regenerate the context pins and use real
consumer policies and retained artifacts for the intended claims. Creating or
attesting work can change native state; do not edit an old pin to conceal drift.

Controls:

* Change `record.txt` without repinning: the result is `not_established`.
* Remove a required passage and deliberately repin both case and policy: the
  result is `contradicted`.
* Remove an artifact: the result is `not_established`.
* Change the stored intent definition or selected/inherited view: the old
  manifest is refused before the checker runs.

Run the optional checker's semantic controls in its installed environment:

```sh
/tmp/atomic-evidence-python/bin/python -m unittest discover \
  -s /path/to/atomic/docs/examples/evidence-replay -p test_checker.py -v
```

## Native manifest and checker contract

Capture current pins with:

```sh
atomic intent validate ID --json --evidence-context
```

The additional `evidence_context` contains the native intent URN, its
`intent_substance_hash`, a full stored-source hash (including review state), and
the selected view followed by every ancestor's native Merkle and scope. The chain catches
changes inherited from a parent even when the child's own Merkle is unchanged.
An ID is required; unrecorded markdown files have no native context.

A manifest is a strict JSON object with `schema_version` equal to
`atomic-evidence-replay-manifest/v1`, that exact `context`, and `claims`. Each
claim has `criterion_id`, `claim_type`, `artifact_root`, `case`, and `policy`.
Artifact roots are resolved relative to the manifest. Claims name native criterion
URNs, are unique, and must cover every criterion already marked `met`. Between
1 and 64 claims are allowed. The case and policy are the selected checker's
bounded inputs; the supplied example uses Verify's existing schemas.

Supported claim types are `source_text_coverage/v1`, `operand_lineage/v1`,
`authority_anchor/v1`, and `event_absence/v1`. An arbitrary test-success claim is
unsupported. Adding a new checker capability requires an explicit protocol change
and its own executable positive, negative, and coverage controls.

Atomic invokes the **explicit** `--evidence-checker` executable directly, with no
shell or arguments from the manifest. The executable is trusted local code with
the caller's privileges. Choose it independently of an untrusted evidence bundle;
this mechanism authenticates neither executables nor witnesses. The result's
request digest binds context, criterion, claim, case, policy, and resolved root.
It is an integrity binding, not an authenticated signature or independent custody.

Stdin is `{"request": {...}, "request_digest": "blake3:..."}`. The request has
schema `atomic-evidence-replay-request/v1`, `context`, and `claim`. The checker
returns one strict JSON object with schema
`atomic-evidence-replay-response/v1`, the same `request_digest`, `criterion_id`,
`claim_type`, a `decision` (`supported`, `contradicted`, or `not_established`), a
nonempty `reason`, and `details`. The optional wrapper preserves Verify's actual
scope, checks, and policy digest inside `details`.

Manifest and checker-output bytes use Atomic's existing JSON admission profile
before parsing: repeated members, unsafe or fractional numeric tokens, and excess
nesting are refused. The ordinary gate remains unchanged. The checker gets a
10-second deadline and 1 MiB input/output limits; failure, timeout, invalid binding,
and unknown outcomes cannot pass. Atomic releases repository handles before
invocation and checks native source/view pins again afterwards. This is an
observational read-time check, not a transaction that protects a later dispatch or
repository mutation. Retain the JSON validation report with the manifest and exact
artifacts; it is neither automatically published nor producer acceptance.
