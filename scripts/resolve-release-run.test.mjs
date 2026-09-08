import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'arena0-resolve-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'scripts'));
  mkdirSync(join(root, 'bin'));
  copyFileSync(new URL('./resolve-release-run.mjs', import.meta.url), join(root, 'scripts/resolve-release-run.mjs'));
  writeFileSync(join(root, 'Cargo.toml'), '[workspace.package]\nversion = "1.2.3"\n');
  for (const args of [['init', '-q'], ['add', '.'], ['-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'fixture']]) {
    assert.equal(spawnSync('git', args, { cwd: root }).status, 0);
  }
  const revision = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).stdout.trim();
  const candidate = {
    id: 42, head_sha: revision, head_branch: 'main', event: 'push', status: 'completed', conclusion: 'success',
    path: '.github/workflows/ci.yml', repository: { full_name: 'owner/arena0' }, head_repository: { full_name: 'owner/arena0' },
  };
  writeFileSync(join(root, 'bin/gh'), `#!${process.execPath}\nimport('node:fs').then(fs => {
    fs.writeFileSync(process.env.CALL_LOG, JSON.stringify(process.argv.slice(2)));
    console.log(process.env.GH_RESPONSE);
  });\n`, { mode: 0o755 });
  return {
    root, candidate,
    run(response, env = {}) {
      return spawnSync(process.execPath, [join(root, 'scripts/resolve-release-run.mjs')], {
        encoding: 'utf8', env: { ...process.env, PATH: `${root}/bin:${process.env.PATH}`,
          GITHUB_REPOSITORY: 'owner/arena0', GITHUB_OUTPUT: '', GITHUB_REF_TYPE: 'branch', GITHUB_REF_NAME: 'main',
          RELEASE_VERSION: '', CI_RUN_ID: '', CALL_LOG: join(root, 'calls.json'), GH_RESPONSE: JSON.stringify(response), ...env },
      });
    },
  };
}

test('automatic selection uses only successful main CI for the exact checkout', t => {
  const f = fixture(t);
  const result = f.run([{ workflow_runs: [{ ...f.candidate, id: 99, head_sha: 'wrong' }, f.candidate] }]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), { run_id: 42, revision: f.candidate.head_sha, version: '1.2.3' });
  const args = JSON.parse(readFileSync(join(f.root, 'calls.json'), 'utf8'));
  assert.ok(args.includes('--paginate'));
  assert.match(args.at(-1), new RegExp(`head_sha=${f.candidate.head_sha}&branch=main&event=push&status=success`));
});

test('explicit matching run is accepted and tag and requested version agree', t => {
  const f = fixture(t);
  const result = f.run(f.candidate, { CI_RUN_ID: '42', GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v1.2.3', RELEASE_VERSION: '1.2.3' });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).run_id, 42);
});

for (const [name, change] of Object.entries({
  'wrong commit': { head_sha: 'other' }, 'wrong branch': { head_branch: 'feature' },
  'PR event': { event: 'pull_request' }, 'unfinished run': { status: 'in_progress' },
  'failed run': { conclusion: 'failure' }, 'other workflow': { path: '.github/workflows/release.yml' },
  'other repository': { repository: { full_name: 'other/arena0' } },
  'fork source': { head_repository: { full_name: 'fork/arena0' } }, 'wrong run ID': { id: 43 },
})) {
  test(`explicit selection rejects ${name}`, t => {
    const f = fixture(t);
    const result = f.run({ ...f.candidate, ...change }, { CI_RUN_ID: '42' });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Requested run is not successful/);
  });
}

test('no exact successful run fails without falling back to another commit', t => {
  const f = fixture(t);
  const result = f.run([{ workflow_runs: [] }]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Wait for CI/);
});

for (const env of [{ RELEASE_VERSION: '9.0.0' }, { GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v9.0.0' }, { CI_RUN_ID: '../42' }]) {
  test(`invalid release identity fails before GitHub lookup: ${JSON.stringify(env)}`, t => {
    const f = fixture(t);
    const result = f.run(f.candidate, env);
    assert.notEqual(result.status, 0);
    assert.throws(() => readFileSync(join(f.root, 'calls.json')), { code: 'ENOENT' });
  });
}
