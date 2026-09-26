#!/usr/bin/env node
// Build artifacts are packed once; publication consumes those exact tarballs.
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const platforms = ['darwin-arm64', 'linux-x64'];
const binaries = ['arena0', 'arena0d', 'cargo-arena0'];
const json = path => JSON.parse(readFileSync(path, 'utf8'));
const digest = (file, algorithm, encoding = 'hex') => createHash(algorithm).update(readFileSync(file)).digest(encoding);
const workspaceVersion = () => readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/\[workspace.package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1];

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, ...options });
  if (result.error || result.status !== 0) throw new Error(result.error?.message ?? `${command}: ${result.stderr}`);
  if (result.stderr) process.stderr.write(result.stderr);
  return result.stdout;
}

function verifyArtifacts(directory, selected, revision, version) {
  let dirty = false;
  for (const platform of selected) {
    const folder = join(directory, platform);
    const manifest = json(join(folder, 'manifest.json'));
    if (manifest.platform !== platform || manifest.revision !== revision || manifest.version !== version || typeof manifest.dirty !== 'boolean') {
      throw new Error(`${platform}: artifact identity does not match ${revision} / ${version}`);
    }
    dirty ||= manifest.dirty;
    for (const binary of binaries) {
      const file = join(folder, binary);
      if (digest(file, 'sha256') !== manifest.binaries?.[binary]) throw new Error(`${platform}/${binary}: checksum mismatch`);
      chmodSync(file, 0o755); // ZIP artifact downloads lose executable permissions.
    }
  }
  return dirty;
}

function pack(artifactDirectory, outputDirectory, platform) {
  if (!artifactDirectory || !outputDirectory || (platform && !platforms.includes(platform))) {
    throw new Error('Usage: node npm/release.mjs pack <artifacts> <new-output-directory> [linux-x64|darwin-arm64]');
  }
  const selected = platform ? [platform] : platforms;
  const revision = run('git', ['rev-parse', 'HEAD']).trim();
  const version = workspaceVersion();
  const artifactDirty = verifyArtifacts(resolve(artifactDirectory), selected, revision, version);
  const dirty = artifactDirty || run('git', ['status', '--porcelain', '--untracked-files=no']).trim() !== '';
  const output = resolve(outputDirectory);
  if (existsSync(output)) throw new Error(`Output already exists: ${output}; choose a new directory to preserve packed artifacts.`);
  mkdirSync(output, { recursive: true });
  const staging = mkdtempSync(join(tmpdir(), 'arena0-npm-stage-'));
  const packages = [];
  try {
    for (const target of selected) {
      process.stdout.write(run(process.execPath, [join(root, 'npm/assemble.mjs'), target, resolve(artifactDirectory, target), version, staging]));
    }
    for (const packageName of [...selected.map(target => `arena0-${target}`), 'arena0']) {
      const [packed] = JSON.parse(run('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', output, join(staging, packageName)]));
      const expectedFiles = packageName === 'arena0'
        ? ['bin/arena0.js', 'bin/arena0d.js', 'bin/cargo-arena0.js', 'bin/launch.js', 'examples/minimal-program/Cargo.toml', 'examples/minimal-program/src/lib.rs', 'examples/minimal-program/README.md', 'examples/minimal-program/rust-toolchain.toml']
        : binaries.map(binary => `bin/${binary}`);
      expectedFiles.push('package.json', 'README.md', 'LICENSE-APACHE', 'LICENSE-MIT');
      for (const file of expectedFiles) {
        const entry = packed.files.find(entry => entry.path === file);
        if (!entry || entry.size === 0) throw new Error(`${packed.name}: missing or empty ${file}`);
        if (packageName !== 'arena0' && file.startsWith('bin/') && !(entry.mode & 0o111)) throw new Error(`${packed.name}: ${file} is not executable`);
      }
      if (packed.version !== version) throw new Error(`${packed.name}: wrong package version`);
      const integrity = `sha512-${digest(join(output, packed.filename), 'sha512', 'base64')}`;
      if (integrity !== packed.integrity) throw new Error(`${packed.name}: tarball integrity mismatch`);
      packages.push({ name: packed.name, filename: packed.filename, integrity });
      console.log(`Packed ${packed.name}@${version}: ${packed.size} bytes`);
    }
    writeFileSync(join(output, 'manifest.json'), JSON.stringify({ revision, version, dirty, platforms: selected, packages }, null, 2) + '\n');
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
  console.log(`Packed release: ${output}`);
}

function publish(directory, mode = '--dry-run') {
  if (!directory || !['--publish', '--dry-run'].includes(mode)) throw new Error('Usage: node npm/release.mjs publish <packed-directory> [--dry-run|--publish]');
  const output = resolve(directory);
  const manifest = json(join(output, 'manifest.json'));
  const revision = run('git', ['rev-parse', 'HEAD']).trim();
  if (manifest.revision !== revision || manifest.version !== workspaceVersion()) throw new Error('Packed release does not match this checkout.');
  if (!Array.isArray(manifest.platforms) || manifest.platforms.length === 0 || new Set(manifest.platforms).size !== manifest.platforms.length || manifest.platforms.some(platform => !platforms.includes(platform))) throw new Error('Invalid packed platform set.');
  const expectedNames = [...platforms.filter(platform => manifest.platforms.includes(platform)).map(platform => `@0xff-ai/arena0-${platform}`), '@0xff-ai/arena0'];
  if (JSON.stringify(manifest.packages?.map(pkg => pkg.name)) !== JSON.stringify(expectedNames)) throw new Error('Invalid package set or publication order.');
  const dryRun = mode === '--dry-run';
  if (!dryRun && (manifest.dirty !== false || manifest.platforms.length !== platforms.length)) throw new Error('Publication requires clean, verified artifacts for every supported platform.');
  if (!dryRun && run('git', ['status', '--porcelain', '--untracked-files=no']).trim()) throw new Error('Publication requires a clean checkout.');
  const packages = manifest.packages.map(pkg => {
    if (basename(pkg.filename) !== pkg.filename || !pkg.filename.endsWith('.tgz')) throw new Error('Invalid tarball filename.');
    const file = join(output, pkg.filename);
    if (`sha512-${digest(file, 'sha512', 'base64')}` !== pkg.integrity) throw new Error(`${pkg.name}: packed tarball changed after validation`);
    return { ...pkg, file };
  });
  // Preflight every immutable name/version before the first registry write.
  // A retry may skip identical uploads, never a different existing tarball.
  const existing = new Set();
  if (!dryRun) {
    for (const pkg of packages) {
      const result = spawnSync('npm', ['view', `${pkg.name}@${manifest.version}`, 'dist.integrity', '--json', '--registry=https://registry.npmjs.org'], { encoding: 'utf8' });
      if (result.error) throw result.error;
      if (result.status === 0) {
        if (JSON.parse(result.stdout) !== pkg.integrity) throw new Error(`${pkg.name}@${manifest.version} already exists with different bytes.`);
        existing.add(pkg.name);
      } else {
        let error;
        try { error = JSON.parse(result.stdout).error; } catch {}
        if (error?.code !== 'E404') throw new Error(`Cannot check ${pkg.name}: ${result.stderr}`);
      }
    }
  }
  const tag = manifest.version.includes('-') ? 'dev' : 'latest';
  for (const pkg of packages) {
    if (existing.has(pkg.name)) { console.log(`${pkg.name} already published with matching integrity`); continue; }
    const args = ['publish', pkg.file, '--ignore-scripts', '--access', 'public', '--provenance', '--tag', tag, '--registry=https://registry.npmjs.org'];
    if (dryRun) args.push('--dry-run');
    process.stdout.write(run('npm', args));
  }
  console.log(dryRun ? 'Dry run complete; no packages published.' : 'Release published.');
}

try {
  const [command, ...args] = process.argv.slice(2);
  if (command === 'pack' && args.length <= 3) pack(...args);
  else if (command === 'publish' && args.length <= 2) publish(...args);
  else throw new Error('Usage: node npm/release.mjs <pack|publish> ...');
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
