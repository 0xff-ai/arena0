#!/usr/bin/env node
// Select only a successful main-push CI run for this exact release commit.
import { spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  if (result.error || result.status !== 0) throw new Error(result.error?.message ?? `${command}: ${result.stderr}`);
  return result.stdout.trim();
}

try {
  const repository = process.env.GITHUB_REPOSITORY;
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository ?? '')) throw new Error('GITHUB_REPOSITORY must identify owner/repository.');
  const revision = run('git', ['rev-parse', 'HEAD']);
  const version = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/\[workspace.package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1];
  if (process.env.GITHUB_REF_TYPE === 'tag' && process.env.GITHUB_REF_NAME !== `v${version}`) {
    throw new Error(`Release tag must be v${version}.`);
  }
  const valid = candidate => candidate.head_sha === revision && candidate.head_branch === 'main'
    && candidate.event === 'push' && candidate.status === 'completed' && candidate.conclusion === 'success'
    && candidate.path === '.github/workflows/ci.yml'
    && candidate.repository?.full_name?.toLowerCase() === repository.toLowerCase()
    && candidate.head_repository?.full_name?.toLowerCase() === repository.toLowerCase()
    && Number.isSafeInteger(candidate.id) && candidate.id > 0;
  const pages = JSON.parse(run('gh', ['api', '--paginate', '--slurp',
    `repos/${repository}/actions/workflows/ci.yml/runs?head_sha=${revision}&branch=main&event=push&status=success&per_page=100`]));
  const selected = pages.flatMap(page => page.workflow_runs).filter(valid).sort((a, b) => b.id - a.id)[0];
  if (!selected) throw new Error(`No successful main-push CI run for ${revision}. Wait for CI, then rerun Release at this commit; artifacts are never taken from another commit.`);
  const result = { run_id: selected.id, revision, version };
  if (process.env.GITHUB_OUTPUT) {
    appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(result).map(([key, value]) => `${key}=${value}\n`).join(''));
  }
  console.log(JSON.stringify(result));
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
