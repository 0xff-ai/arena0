#!/usr/bin/env node
// Assemble an arena0 platform package from the built public executables, and sync versions
// across the main + platform package.json files.
//
//   node npm/assemble.mjs <target> <binary-directory> [version] [output-directory]
//
//   target       one of: darwin-arm64, linux-x64
//   binary-directory  directory containing `arena0`, `arena0d`, and `cargo-arena0`
//   version      optional; defaults to the workspace version in Cargo.toml
//   output-directory  optional staging tree; defaults to this npm directory
//
// release.mjs invokes this for each verified target in a temporary staging tree.
// Versions propagate to the main package's optionalDependencies so the pins
// always match. This script only assembles files; it does not verify provenance
// or publish packages.
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const TARGETS = new Set(['darwin-arm64', 'linux-x64']);
const BINARIES = ['arena0', 'arena0d', 'cargo-arena0'];
const sourceNpmDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(sourceNpmDir, '..');

const [target, binaryDirectory, versionArg, outputDirectory] = process.argv.slice(2);
const npmDir = outputDirectory ? resolve(outputDirectory) : sourceNpmDir;
if (!target || !binaryDirectory) {
  console.error('usage: node npm/assemble.mjs <target> <binary-directory> [version] [output-directory]');
  process.exit(2);
}
if (!TARGETS.has(target)) {
  console.error(`unknown target ${target}; expected one of ${[...TARGETS].join(', ')}`);
  process.exit(2);
}

const workspace = workspaceVersion();
const version = versionArg ?? workspace;
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`invalid release version ${JSON.stringify(version)}`);
  process.exit(2);
}
if (version !== workspace) {
  console.error(
    `release version ${version} does not match Cargo workspace version ${workspace}`,
  );
  process.exit(2);
}

// Copy all public executables into the platform package.
const sourceDir = resolve(binaryDirectory);
if (!existsSync(sourceDir) || !statSync(sourceDir).isDirectory()) {
  console.error(`binary directory is not a directory: ${binaryDirectory}`);
  process.exit(2);
}
if (target === 'linux-x64') {
  const check = spawnSync(join(repoRoot, 'scripts', 'check-linux-artifacts.sh'), [sourceDir], {
    stdio: 'inherit',
  });
  if (check.status !== 0) {
    process.exit(check.status ?? 1);
  }
}
const binDir = join(npmDir, `arena0-${target}`, 'bin');
// Release packing uses a separate staging tree. Copy authored package inputs
// only; ignored binaries or tarballs from a previous local build must not leak in.
if (npmDir !== sourceNpmDir) {
  for (const packageDir of ['arena0', 'arena0-darwin-arm64', 'arena0-linux-x64']) {
    mkdirSync(join(npmDir, packageDir), { recursive: true });
    for (const file of ['package.json', 'README.md']) {
      copyFileSync(join(sourceNpmDir, packageDir, file), join(npmDir, packageDir, file));
    }
  }
  mkdirSync(join(npmDir, 'arena0', 'bin'), { recursive: true });
  for (const file of ['arena0.js', 'arena0d.js', 'cargo-arena0.js', 'launch.js']) {
    copyFileSync(join(sourceNpmDir, 'arena0', 'bin', file), join(npmDir, 'arena0', 'bin', file));
  }
}
mkdirSync(binDir, { recursive: true });
for (const binary of BINARIES) {
  const source = join(sourceDir, binary);
  if (!existsSync(source) || !statSync(source).isFile()) {
    console.error(`missing executable: ${source}`);
    process.exit(2);
  }
  const dest = join(binDir, binary);
  copyFileSync(source, dest);
  chmodSync(dest, 0o755);
  console.log(`copied ${source} -> ${dest}`);
}

// The main package carries one copyable, version-only program example. Copy
// only its authored inputs so local Cargo artifacts can never enter a tarball.
const exampleSource = join(repoRoot, 'examples', 'minimal-program');
const exampleDest = join(npmDir, 'arena0', 'examples', 'minimal-program');
const exampleFiles = ['Cargo.toml', 'README.md', 'rust-toolchain.toml', 'src/lib.rs'];
rmSync(exampleDest, { recursive: true, force: true });
for (const file of exampleFiles) {
  const source = join(exampleSource, file);
  const dest = join(exampleDest, file);
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(source, dest);
}
console.log(`copied minimal program example -> ${exampleDest}`);

// Every published tarball carries arena0's licenses.
for (const packageDir of ['arena0', 'arena0-darwin-arm64', 'arena0-linux-x64']) {
  for (const notice of ['LICENSE-APACHE', 'LICENSE-MIT']) {
    copyFileSync(join(repoRoot, notice), join(npmDir, packageDir, notice));
  }
}

// Sync versions across every package.json.
setVersion(join(npmDir, 'arena0', 'package.json'), version, true);
setVersion(join(npmDir, 'arena0-darwin-arm64', 'package.json'), version, false);
setVersion(join(npmDir, 'arena0-linux-x64', 'package.json'), version, false);
console.log(`set version ${version}`);

function workspaceVersion() {
  const toml = readFileSync(join(repoRoot, 'Cargo.toml'), 'utf8');
  const match = toml.match(/\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/);
  if (!match) {
    console.error('could not read [workspace.package] version from Cargo.toml');
    process.exit(1);
  }
  return match[1];
}

function setVersion(pkgPath, version, isMain) {
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
  pkg.version = version;
  if (isMain && pkg.optionalDependencies) {
    for (const dep of Object.keys(pkg.optionalDependencies)) {
      pkg.optionalDependencies[dep] = version;
    }
  }
  writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`);
}
