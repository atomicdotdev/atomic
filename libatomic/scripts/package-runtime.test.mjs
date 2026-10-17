import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { test } from 'node:test';
import { packageRuntime, targets } from './package-runtime.mjs';

const revision = 'a'.repeat(40);
function fixture(t, target = 'aarch64-apple-darwin') {
  const dir = mkdtempSync(join(tmpdir(), 'libatomic-runtime-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const spec = targets[target];
  writeFileSync(join(dir, spec.source), 'native fixture');
  return {
    options: { target, buildDir: dir, outputDir: join(dir, 'runtime'), version: '0.19.1', revision },
    dependencies: {
      host: { platform: spec.platform, arch: spec.arch, versions: { napi: '8' } },
      load: (path) => {
        assert.equal(basename(path), spec.library);
        assert.equal(readFileSync(path, 'utf8'), 'native fixture');
        return { call() {}, descriptor: () => Buffer.from('descriptor fixture') };
      },
    },
  };
}

for (const [target, spec] of Object.entries(targets)) {
  test(`normalizes ${target} and records release identity and checksums`, (t) => {
    const { options, dependencies } = fixture(t, target);
    const metadata = packageRuntime(options, dependencies);
    assert.deepEqual(metadata, {
      schemaVersion: 1, version: '0.19.1', revision, target,
      platform: spec.platform, arch: spec.arch, nodeApiVersion: 8, library: spec.library,
      librarySha256: createHash('sha256').update('native fixture').digest('hex'),
      descriptorSha256: createHash('sha256').update('descriptor fixture').digest('hex'),
    });
    assert.deepEqual(JSON.parse(readFileSync(join(options.outputDir, 'libatomic-runtime.json'))), metadata);
    assert.equal(readFileSync(join(options.buildDir, spec.source), 'utf8'), 'native fixture');
  });
}

test('rejects unsupported, Intel Mac and musl addon targets', (t) => {
  const { options, dependencies } = fixture(t);
  for (const target of ['x86_64-apple-darwin', 'x86_64-unknown-linux-musl', '../bad', '__proto__']) {
    assert.throws(() => packageRuntime({ ...options, target }, dependencies), /Unsupported/);
  }
  assert.equal(existsSync(options.outputDir), false);
});

test('refuses mismatched host OS or CPU and missing Node-API support', (t) => {
  const { options, dependencies } = fixture(t);
  for (const host of [
    { platform: 'darwin', arch: 'x64', versions: { napi: '8' } },
    { platform: 'linux', arch: 'arm64', versions: { napi: '8' } },
    { platform: 'darwin', arch: 'arm64', versions: { napi: '7' } },
    { platform: 'darwin', arch: 'arm64', versions: {} },
  ]) {
    assert.throws(() => packageRuntime(options, { ...dependencies, host }), /requires/);
  }
  assert.equal(existsSync(options.outputDir), false);
});

test('requires version and full source revision', (t) => {
  const { options, dependencies } = fixture(t);
  assert.throws(() => packageRuntime({ ...options, version: '' }, dependencies), /version/);
  assert.throws(() => packageRuntime({ ...options, revision: 'abc' }, dependencies), /revision/);
  assert.equal(existsSync(options.outputDir), false);
});

test('missing build does not create runtime metadata', (t) => {
  const { options, dependencies } = fixture(t);
  rmSync(join(options.buildDir, targets[options.target].source));
  assert.throws(() => packageRuntime(options, dependencies), /ENOENT/);
  assert.equal(existsSync(options.outputDir), false);
});

test('does not overwrite an already staged or mapped runtime', (t) => {
  const { options, dependencies } = fixture(t);
  packageRuntime(options, dependencies);
  assert.throws(() => packageRuntime(options, dependencies), /EEXIST/);
  assert.equal(readFileSync(join(options.outputDir, targets[options.target].library), 'utf8'), 'native fixture');
});

for (const [name, load, message] of [
  ['load failure', () => { throw new Error('invalid native binary'); }, /invalid native binary/],
  ['non-Node library', () => ({}), /features node/],
  ['missing descriptor', () => ({ call() {}, descriptor: () => Buffer.alloc(0) }), /no protobuf descriptor/],
]) {
  test(`rejects ${name} without writing a manifest`, (t) => {
    const { options, dependencies } = fixture(t);
    assert.throws(() => packageRuntime(options, { ...dependencies, load }), message);
    assert.equal(existsSync(join(options.outputDir, 'libatomic-runtime.json')), false);
  });
}
