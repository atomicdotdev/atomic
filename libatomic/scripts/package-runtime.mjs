import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { constants, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

// Keep the Rust crate/import name libatomic. Normalize Cargo's extra Unix lib
// prefix only at the distribution boundary, without introducing another crate.
export const targets = Object.freeze({
  'aarch64-apple-darwin': {
    platform: 'darwin', arch: 'arm64', source: 'liblibatomic.dylib', library: 'libatomic.dylib',
  },
  'x86_64-unknown-linux-gnu': {
    platform: 'linux', arch: 'x64', source: 'liblibatomic.so', library: 'libatomic.so',
  },
  'x86_64-pc-windows-msvc': {
    platform: 'win32', arch: 'x64', source: 'libatomic.dll', library: 'libatomic.dll',
  },
});

function loadAddon(path) {
  // require() treats .so/.dylib/.dll as JavaScript. dlopen loads Node-API
  // exports directly while preserving the platform library's installed name.
  const addon = { exports: {} };
  process.dlopen(addon, path);
  return addon.exports;
}

export function packageRuntime(
  { target, buildDir, outputDir, version, revision },
  { host = process, load = loadAddon } = {},
) {
  const spec = Object.hasOwn(targets, target) ? targets[target] : undefined;
  if (!spec) throw new Error(`Unsupported Node runtime target: ${target}`);
  if (host.platform !== spec.platform || host.arch !== spec.arch) {
    throw new Error(`Runtime smoke test requires ${spec.platform}/${spec.arch}, not ${host.platform}/${host.arch}`);
  }
  if (Number(host.versions.napi) < 8 || !host.versions.napi) {
    throw new Error('libatomic requires Node-API 8 or newer');
  }
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error('A release version is required');
  }
  if (!/^[0-9a-f]{40}$/.test(revision)) throw new Error('A full Git source revision is required');

  const source = resolve(buildDir, spec.source);
  // Read before creating the destination so a missing producer is a hard failure.
  readFileSync(source);
  mkdirSync(outputDir, { recursive: true });
  const library = resolve(outputDir, spec.library);
  // Never overwrite a runtime that a process might already have mapped.
  copyFileSync(source, library, constants.COPYFILE_EXCL);
  const addon = load(library);
  if (typeof addon.call !== 'function' || typeof addon.descriptor !== 'function') {
    throw new Error('Library must be built with --features node (missing call/descriptor exports)');
  }
  const descriptor = addon.descriptor();
  if (!Buffer.isBuffer(descriptor) || descriptor.length === 0) {
    throw new Error('Library has no protobuf descriptor');
  }
  const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
  const metadata = {
    schemaVersion: 1,
    version,
    revision,
    target,
    platform: spec.platform,
    arch: spec.arch,
    nodeApiVersion: 8,
    library: spec.library,
    librarySha256: sha256(readFileSync(library)),
    descriptorSha256: sha256(descriptor),
  };
  writeFileSync(resolve(outputDir, 'libatomic-runtime.json'), `${JSON.stringify(metadata, null, 2)}\n`, { flag: 'wx' });
  return metadata;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const { values } = parseArgs({ options: {
    target: { type: 'string' },
    'build-dir': { type: 'string' },
    'output-dir': { type: 'string' },
    'expected-version': { type: 'string' },
  } });
  if (!values.target || !values['build-dir'] || !values['output-dir']) {
    throw new Error('Usage: node libatomic/scripts/package-runtime.mjs --target TARGET --build-dir DIR --output-dir DIR [--expected-version VERSION]');
  }
  const root = fileURLToPath(new URL('../../', import.meta.url));
  const workspace = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--no-deps', '--format-version', '1'], { cwd: root, encoding: 'utf8' }));
  const version = workspace.packages.find((pkg) => pkg.name === 'libatomic').version;
  if (values['expected-version'] && values['expected-version'] !== version) {
    throw new Error(`libatomic version ${version} does not match release ${values['expected-version']}`);
  }
  const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
  const metadata = packageRuntime({
    target: values.target, buildDir: values['build-dir'], outputDir: values['output-dir'], version, revision,
  });
  console.log(`Packaged and Node-load-tested ${metadata.library}: ${metadata.version} (${metadata.target}, ${metadata.revision})`);
}
