# atomic-canonical-wasm

`atomic-canonical` for the browser. A page that holds an atomic identity's
Ed25519 key in WebCrypto signs atomic documents without the key ever leaving
the browser: this module builds the canonical document and the bytes to sign
(with the same code the CLI uses), WebCrypto signs them, and the result is an
ordinary `eddsa-jcs-2022` attestation that `atomic` verifies.

```js
import init, { prepareAttestation, attachProof } from "./pkg/atomic_canonical_wasm.js";
await init();
const prepared = prepareAttestation(JSON.stringify(doc), publicKeyBytes);
const sig = await crypto.subtle.sign("Ed25519", key, prepared.signingBytes);
const attested = JSON.parse(attachProof(prepared.document, publicKeyBytes, new Uint8Array(sig)));
```

Also: `verifyAttestation`, `didForPublicKey`, `publicKeyBase32`, `canonicalize`.

Build with `./build.sh [out-dir]`; `node smoke.mjs` (Node ≥ 22, which has
WebCrypto Ed25519) signs, verifies and tamper-checks a document end to end.
