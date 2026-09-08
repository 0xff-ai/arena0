#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const [platform, source, destination] = process.argv.slice(2);
if (!['linux-x64', 'darwin-arm64'].includes(platform) || !source || !destination) {
  throw new Error('Usage: node scripts/stage-release.mjs <platform> <binary-directory> <output-directory>');
}
if (platform !== `${process.platform}-${process.arch}`) throw new Error('Stage binaries on their native platform so their versions can be checked.');
const run = (command, args) => {
  const result = spawnSync(command, args, { cwd: root, encoding: 'utf8' });
  if (result.error || result.status !== 0) throw new Error(result.error?.message ?? result.stderr);
  return result.stdout.trim();
};
const version = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/\[workspace.package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1];
const manifest = {
  version, platform, revision: run('git', ['rev-parse', 'HEAD']),
  dirty: run('git', ['status', '--porcelain', '--untracked-files=no']) !== '',
  binaries: {},
};
if (platform === 'linux-x64') run(join(root, 'scripts/check-linux-artifacts.sh'), [resolve(source)]);
mkdirSync(destination, { recursive: true });
for (const name of ['arena0', 'arena0d', 'cargo-arena0']) {
  const file = resolve(source, name);
  if (run(file, ['--version']) !== `${name} ${version}`) throw new Error(`${name} version does not match ${version}`);
  const hash = createHash('sha256').update(readFileSync(file)).digest('hex');
  copyFileSync(file, join(destination, name));
  manifest.binaries[name] = hash;
}
writeFileSync(join(destination, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
console.log(`Staged ${platform} ${version} at ${manifest.revision}${manifest.dirty ? ' (dirty checkout; dry run only)' : ''}`);
