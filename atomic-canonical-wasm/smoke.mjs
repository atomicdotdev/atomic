import { readFileSync } from "node:fs";
import init, { prepareAttestation, attachProof, verifyAttestation, didForPublicKey } from "./pkg/atomic_canonical_wasm.js";
await init({ module_or_path: readFileSync(new URL("./pkg/atomic_canonical_wasm_bg.wasm", import.meta.url)) });
const kp = await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"]);
const pub = new Uint8Array(await crypto.subtle.exportKey("raw", kp.publicKey));
const doc = { "@type": "ExampleLogin", nonce: "abc", "é": [1, 2.5, "x"] };
const p = prepareAttestation(JSON.stringify(doc), pub);
const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", kp.privateKey, p.signingBytes));
const attested = attachProof(p.document, pub, sig);
verifyAttestation(attested, pub);
console.log(didForPublicKey(pub));
console.log(attested);
try { verifyAttestation(attested.replace('"abc"', '"abd"'), pub); console.log("TAMPER NOT DETECTED"); } catch (e) { console.log("tamper rejected:", e.message); }
