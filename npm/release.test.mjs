import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'arena0-release-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const dir of ['npm', 'bin', 'packed']) mkdirSync(join(root, dir));
  copyFileSync(new URL('./release.mjs', import.meta.url), join(root, 'npm/release.mjs'));
  writeFileSync(join(root, 'Cargo.toml'), '[workspace.package]\nversion = "1.2.3"\n');
  for (const args of [['init', '-q'], ['add', '.'], ['-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'fixture']]) {
    assert.equal(spawnSync('git', args, { cwd: root }).status, 0);
  }
  const revision = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).stdout.trim();
  const manifest = { revision, version: '1.2.3', dirty: false, platforms: ['darwin-arm64', 'linux-x64'], packages: [] };
  for (const suffix of ['-darwin-arm64', '-linux-x64', '']) {
    const filename = `arena0${suffix}.tgz`;
    const bytes = `test package ${suffix}`;
    writeFileSync(join(root, 'packed', filename), bytes);
    manifest.packages.push({ name: `@0xff-ai/arena0${suffix}`, filename,
      integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}` });
  }
  // All registry commands in this suite hit this executable, never npm.
  writeFileSync(join(root, 'bin/npm'), `#!${process.execPath}\nconst fs = require('node:fs');
    const args = process.argv.slice(2);
    fs.appendFileSync(process.env.CALL_LOG, JSON.stringify(args) + '\\n');
    if (args[0] === 'view') {
      const responses = JSON.parse(process.env.VIEW_RESPONSES || '{}');
      const response = responses[args[1]];
      if (response?.integrity) console.log(JSON.stringify(response.integrity));
      else { console.log(JSON.stringify({ error: { code: response?.error || 'E404' } })); process.exitCode = 1; }
    }
  `, { mode: 0o755 });
  return {
    root, manifest,
    save() { writeFileSync(join(root, 'packed/manifest.json'), JSON.stringify(manifest)); },
    run(args = ['publish', join(root, 'packed')], env = {}) {
      return spawnSync(process.execPath, [join(root, 'npm/release.mjs'), ...args], { encoding: 'utf8',
        env: { ...process.env, PATH: `${root}/bin:${process.env.PATH}`, CALL_LOG: join(root, 'calls.jsonl'), VIEW_RESPONSES: '{}', ...env } });
    },
    calls() { return existsSync(join(root, 'calls.jsonl')) ? readFileSync(join(root, 'calls.jsonl'), 'utf8').trim().split('\n').map(JSON.parse) : []; },
  };
}

test('default publication mode dry-runs the exact tarballs, platforms before wrapper', t => {
  const f = fixture(t); f.save();
  const result = f.run();
  assert.equal(result.status, 0, result.stderr);
  const calls = f.calls();
  assert.equal(calls.length, 3);
  for (const [index, args] of calls.entries()) {
    assert.equal(args[0], 'publish');
    assert.equal(args[1], join(f.root, 'packed', f.manifest.packages[index].filename));
    assert.ok(args.includes('--dry-run'));
    assert.ok(args.includes('--ignore-scripts'));
  }
});

test('publication preflights all packages, then uploads each exact tarball in order', t => {
  const f = fixture(t); f.save();
  const result = f.run(['publish', join(f.root, 'packed'), '--publish']);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(f.calls().map(args => args[0]), ['view', 'view', 'view', 'publish', 'publish', 'publish']);
  assert.deepEqual(f.calls().slice(3).map(args => args[1]), f.manifest.packages.map(pkg => join(f.root, 'packed', pkg.filename)));
  assert.ok(f.calls().slice(3).every(args => !args.includes('--dry-run')));
});

test('retry skips only packages whose registry integrity matches', t => {
  const f = fixture(t); f.save();
  const first = f.manifest.packages[0];
  const result = f.run(['publish', join(f.root, 'packed'), '--publish'], {
    VIEW_RESPONSES: JSON.stringify({ [`${first.name}@1.2.3`]: { integrity: first.integrity } }),
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(f.calls().filter(args => args[0] === 'publish').map(args => args[1]), f.manifest.packages.slice(1).map(pkg => join(f.root, 'packed', pkg.filename)));
});

for (const response of [{ integrity: 'sha512-different' }, { error: 'E403' }]) {
  test(`registry conflict or lookup failure causes no uploads: ${JSON.stringify(response)}`, t => {
    const f = fixture(t); f.save();
    const result = f.run(['publish', join(f.root, 'packed'), '--publish'], {
      VIEW_RESPONSES: JSON.stringify({ '@0xff-ai/arena0@1.2.3': response }),
    });
    assert.notEqual(result.status, 0);
    assert.equal(f.calls().filter(args => args[0] === 'publish').length, 0);
  });
}

for (const [name, mutate] of [
  ['wrong commit', f => { f.manifest.revision = 'other'; }],
  ['wrong version', f => { f.manifest.version = '9.0.0'; }],
  ['missing package', f => { f.manifest.packages.pop(); }],
  ['wrong order', f => { f.manifest.packages.reverse(); }],
  ['tampered tarball', f => writeFileSync(join(f.root, 'packed', f.manifest.packages[0].filename), 'changed')],
  ['path traversal', f => { f.manifest.packages[0].filename = '../outside.tgz'; }],
]) {
  test(`rejects ${name} before invoking npm`, t => {
    const f = fixture(t); mutate(f); f.save();
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.deepEqual(f.calls(), []);
  });
}

for (const [name, mutate] of [
  ['dirty artifacts', f => { f.manifest.dirty = true; }],
  ['dirty checkout', f => writeFileSync(join(f.root, 'Cargo.toml'), '[workspace.package]\nversion = "1.2.3"\n# changed\n')],
  ['single-platform candidate', f => { f.manifest.platforms.shift(); f.manifest.packages.shift(); }],
]) {
  test(`${name} permits dry run but forbids publication`, t => {
    const f = fixture(t); mutate(f); f.save();
    const blocked = f.run(['publish', join(f.root, 'packed'), '--publish']);
    assert.notEqual(blocked.status, 0);
    assert.deepEqual(f.calls(), []);
    const dryRun = f.run();
    assert.equal(dryRun.status, 0, dryRun.stderr);
  });
}

for (const problem of ['missing manifest', 'wrong revision', 'wrong version', 'wrong checksum', 'missing second platform']) {
  test(`packing fails closed for ${problem}`, t => {
    const f = fixture(t);
    const dir = join(f.root, 'dist/darwin-arm64'); mkdirSync(dir, { recursive: true });
    const artifact = { platform: 'darwin-arm64', version: '1.2.3', revision: f.manifest.revision, dirty: false, binaries: {} };
    for (const binary of ['arena0', 'arena0d', 'cargo-arena0']) {
      writeFileSync(join(dir, binary), binary);
      artifact.binaries[binary] = createHash('sha256').update(binary).digest('hex');
    }
    if (problem === 'wrong revision') artifact.revision = 'other';
    if (problem === 'wrong version') artifact.version = '9.0.0';
    if (problem === 'wrong checksum') artifact.binaries.arena0 = 'incorrect';
    if (problem !== 'missing manifest') writeFileSync(join(dir, 'manifest.json'), JSON.stringify(artifact));
    const output = join(f.root, 'candidate');
    const args = ['pack', join(f.root, 'dist'), output];
    if (problem !== 'missing second platform') args.push('darwin-arm64');
    const result = f.run(args);
    assert.notEqual(result.status, 0);
    assert.ok(!existsSync(output));
    assert.deepEqual(f.calls(), []);
  });
}
