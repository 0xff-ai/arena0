'use strict';

const { spawn } = require('node:child_process');

module.exports = function launch(binaryName) {
  const { platform, arch } = process;
  const pkg = `@0xff-ai/arena0-${platform}-${arch}`;
  const executable = platform === 'win32' ? `${binaryName}.exe` : binaryName;
  let binary;
  try {
    binary = require.resolve(`${pkg}/bin/${executable}`);
  } catch {
    console.error(`arena0: no prebuilt ${binaryName} binary for ${platform}-${arch}.`);
    console.error('Prebuilt targets: darwin-arm64, linux-x64.');
    console.error('Build from source: https://github.com/0xff-ai/arena0');
    process.exit(1);
  }

  const child = spawn(binary, process.argv.slice(2), {
    detached: platform !== 'win32',
    stdio: 'inherit',
  });
  const forward = (signal) => {
    if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
    try {
      if (platform === 'win32') child.kill(signal);
      else process.kill(-child.pid, signal);
    } catch (error) {
      if (error.code !== 'ESRCH') throw error;
    }
  };
  const onInterrupt = () => forward('SIGINT');
  const onTerminate = () => forward('SIGTERM');
  process.on('SIGINT', onInterrupt);
  process.on('SIGTERM', onTerminate);

  child.once('error', (error) => {
    console.error(`arena0: failed to run ${binary}: ${error.message}`);
    process.exitCode = 1;
  });
  child.once('exit', (code, signal) => {
    process.removeListener('SIGINT', onInterrupt);
    process.removeListener('SIGTERM', onTerminate);
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exitCode = code ?? 1;
  });
};
