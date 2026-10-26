import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';

// CI cache keys and the local stamp must both cover these inputs.
export const nativeBuildInputs = [
  'scripts/prepare-libzt.mjs',
  'scripts/patch-libzt.mjs',
  'scripts/patch-libzt-packet-adapter.mjs',
  'scripts/libzt-packet-adapter/api.h',
  'scripts/libzt-packet-adapter/LinkPacketAdapter.hpp',
  'scripts/libzt-packet-adapter/LinkPacketAdapter.cpp',
  'scripts/diagnostics/actual-socket-stun.hpp',
  'scripts/libzt-build-config.mjs',
];

export function libztBuildPlan(root, platform = process.platform, arch = process.arch) {
  const generator = platform === 'win32' ? 'Ninja' : null;
  const hash = createHash('sha256').update(`${platform}:${arch}:${generator ?? 'default'}`);
  for (const input of nativeBuildInputs) hash.update(input).update(fs.readFileSync(path.join(root, input)));
  return {
    generator,
    // Never reconfigure an existing NMake cache as Ninja.
    directory: generator ? 'planet-node-native-release-ninja' : 'planet-node-native-release',
    fingerprint: hash.digest('hex'),
  };
}

export function windowsNinjaEnv(env = process.env, exists = fs.existsSync) {
  // Ninja otherwise prefers a PATH-provided cc.exe (e.g. MSYS GCC), which is
  // incompatible with our MSVC flags and Rust windows-msvc static linkage.
  const result = {...env, CMAKE_GENERATOR: 'Ninja', CC: 'cl', CXX: 'cl'};
  // Windows preserves the parent's spelling (including lowercase `path`).
  // Once spread into a plain object, lookups are case-sensitive; duplicate
  // spellings can also make Node discard the inherited search path.
  const pathKey = Object.keys(env).find(key => key.toLowerCase() === 'path');
  const inheritedPath = env.PATH ?? env.Path ?? (pathKey ? env[pathKey] : '') ?? '';
  for (const key of Object.keys(result)) if (key.toLowerCase() === 'path') delete result[key];
  result.PATH = inheritedPath;
  // vcvars/MSVC dev-cmd exposes VSINSTALLDIR, but not always bundled Ninja.
  const ninja = env.VSINSTALLDIR && path.join(env.VSINSTALLDIR, 'Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja');
  if (ninja && exists(path.join(ninja, 'ninja.exe'))) result.PATH = `${ninja};${inheritedPath}`;
  return result;
}

// Preserve ordinary-terminal development: unlike the Visual Studio generator,
// Ninja needs INCLUDE/LIB and the MSVC tools initialized before configuration.
export function windowsMsvcEnv(env = process.env, {
  exists = fs.existsSync,
  run = (command, args, childEnv) => execFileSync(command, args, {
    env: childEnv, encoding: 'utf8', windowsHide: true, maxBuffer: 2 * 1024 * 1024,
    // cmd.exe parses its own command string; Win32 argv quoting would turn the
    // quoted batch path into literal backslash-quotes (including paths with spaces).
    windowsVerbatimArguments: /(?:^|[\\/])cmd\.exe$/i.test(command),
  }),
} = {}) {
  if (env.VSCMD_VER && env.INCLUDE && env.LIB) return {...env};
  let installation = env.VSINSTALLDIR;
  if (!installation) {
    const vswhere = path.join(env['ProgramFiles(x86)'] ?? 'C:/Program Files (x86)', 'Microsoft Visual Studio/Installer/vswhere.exe');
    if (!exists(vswhere)) throw new Error('MSVC tools not found. Install Visual Studio C++ Build Tools or use its x64 developer terminal.');
    installation = run(vswhere, ['-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath'], env).trim();
  }
  const vcvars = path.join(installation, 'VC/Auxiliary/Build/vcvars64.bat');
  if (!installation || !exists(vcvars)) throw new Error('MSVC x64 developer tools are missing. Install the Visual Studio C++ workload.');
  // Keep paths in the child environment rather than interpolating shell code.
  let output;
  try {output = run(env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', 'call "%LINK_BUILD_VCVARS%" >nul && set'], {...env, LINK_BUILD_VCVARS: vcvars});}
  catch {throw new Error('MSVC environment initialization failed. Try the Visual Studio x64 developer terminal.');}
  const result = {...env};
  for (const line of output.split(/\r?\n/)) {
    const at = line.indexOf('='); if (at <= 0) continue;
    const key = line.slice(0, at);
    for (const old of Object.keys(result)) if (old !== key && old.toLowerCase() === key.toLowerCase()) delete result[old];
    result[key] = line.slice(at + 1);
  }
  delete result.LINK_BUILD_VCVARS;
  if (!result.INCLUDE || !result.LIB) throw new Error('MSVC initialization did not provide INCLUDE and LIB.');
  return result;
}
