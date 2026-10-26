import {libztBuildPlan, windowsNinjaEnv, windowsMsvcEnv} from './libzt-build-config.mjs';
import{createHash}from'node:crypto';
import {patchLibzt} from "./patch-libzt.mjs";
import { existsSync, mkdirSync, copyFileSync, readdirSync, statSync, readFileSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, join } from 'node:path';

const dependencyLock = JSON.parse(readFileSync(new URL('../native/dependencies.json', import.meta.url), 'utf8'));
const sdkRevision = dependencyLock.libzt.revision;
const root = resolve(import.meta.dirname, '..');
const vend = join(root, 'vendor', 'libzt');
const manifest = join(vend, 'pkg', 'crate', 'libzt', 'Cargo.toml');
const nativeDir = join(vend, 'dist', 'native', 'lib');
const nativeFile = join(nativeDir, process.platform === 'win32' ? 'zt.lib' : 'libzt.a');
const plan = libztBuildPlan(root);
let nativeEnv = process.env;
const cmakeBuild = join(vend, 'cache', plan.directory);
const nativeBuildStamp = join(nativeDir, 'build-revision');
const nativeBuildRevision = 'linksdk-native-v1-' + process.platform + '-' + process.arch + '-' + sdkRevision + '-' + plan.fingerprint;
const jsonDir = join(root, 'vendor', 'nlohmann-json');
const jsonHeader = join(jsonDir, 'single_include', 'nlohmann', 'json.hpp');

function run(cmd, args, cwd = root) {
  console.log(`> ${cmd} ${args.join(' ')}`);
  execFileSync(cmd, args, { cwd, stdio: 'inherit', env: nativeEnv });
}
function findLibrary(dir, depth = 0) {
  if (!existsSync(dir) || depth > 7) return null;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      const hit = findLibrary(p, depth + 1);
      if (hit) return hit;
    } else if (entry.isFile()) {
      const x = entry.name.toLowerCase();
      const expected = process.platform === 'win32' ? ['zt-static.lib', 'zt.lib', 'libzt.lib'] : ['libzt.a'];
      if (expected.includes(x) && statSync(p).size > 0) return p;
    }
  }
  return null;
}

try {
  for (const [file, expected] of Object.entries(dependencyLock.patchInputs)) {
    const actual = createHash('sha256').update(readFileSync(join(root, file))).digest('hex');
    if (actual !== expected) throw Error(`Patch input mismatch: ${file}; review and update native/dependencies.json`);
  }
  if (!existsSync(manifest)) {
    mkdirSync(join(root, 'vendor'), { recursive: true });
    run('git', ['clone', '--no-checkout', '--depth', '1', 'https://github.com/zerotier/libzt.git', vend]);
    run('git', ['fetch', '--depth', '1', 'origin', sdkRevision], vend);
    run('git', ['checkout', '--detach', sdkRevision], vend);
    run('git', ['submodule', 'update', '--init', '--recursive'], vend);
  }
  if (!existsSync(join(vend, 'ext', 'ZeroTierOne', 'CMakeLists.txt')) ||
      !existsSync(join(vend, 'ext', 'lwip', 'src'))) {
    run('git', ['submodule', 'update', '--init', '--recursive'], vend);
  }
  const actualRevision = execFileSync('git', ['rev-parse', 'HEAD'], {cwd:vend,encoding:'utf8'}).trim();
  if(actualRevision !== sdkRevision) throw Error(`libzt revision mismatch: ${actualRevision}; expected ${sdkRevision}`);
  for (const [path, expected] of Object.entries(dependencyLock.libzt.submodules)) {
    const actual = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: join(vend, path), encoding: 'utf8' }).trim();
    if (actual !== expected) throw Error(`Submodule revision mismatch: ${path}`);
  }
  patchLibzt(vend);
  if (!existsSync(jsonHeader)) {
    run('git', ['clone', '--depth', '1', '--branch', 'v3.11.3',
      'https://github.com/nlohmann/json.git', jsonDir]);
  }
  if (!existsSync(jsonHeader)) throw new Error(`nlohmann header missing: ${jsonHeader}`);
  if (createHash('sha256').update(readFileSync(jsonHeader)).digest('hex') !== dependencyLock['nlohmann-json'].headerSha256) throw Error('nlohmann header hash mismatch');
  if (process.argv.includes('--android')) {
    console.log('libzt sources ready for Android; native build is target-specific.');
    process.exit(0);
  }
  if (!existsSync(nativeFile) || !existsSync(nativeBuildStamp) ||
      readFileSync(nativeBuildStamp, 'utf8') !== nativeBuildRevision) {
    const flags = [
      '-S', vend, '-B', cmakeBuild,
      '-DCMAKE_BUILD_TYPE=Release',
      '-DCMAKE_POLICY_VERSION_MINIMUM=3.5',
      '-DZTS_ENABLE_RUST=ON',
      '-DBUILD_HOST=OFF',
      '-DZTS_DISABLE_CENTRAL_API=ON',
    ];
    if (plan.generator) {
      nativeEnv = windowsNinjaEnv(windowsMsvcEnv());
      // Fail clearly rather than silently returning to a serial generator.
      try {execFileSync('ninja', ['--version'], {env: nativeEnv, stdio: 'pipe'});}
      catch {throw new Error('Windows libzt builds require Ninja on PATH or the Visual Studio CMake tools; serial NMake fallback is disabled.');}
      flags.push('-G', plan.generator);
    }
    // The embedded official Controller also depends on nlohmann/json.hpp.
    const includeDir = join(jsonDir, 'single_include').replaceAll('\\', '/');
    if (process.platform === 'win32') {
      flags.push(`-DCMAKE_CXX_FLAGS:STRING=/I\"${includeDir}\"`);
      flags.push('-DCMAKE_C_FLAGS:STRING=/DLWIP_RAW=1 /DLWIP_SO_RCVTIMEO=1');
    } else {
      flags.push(`-DCMAKE_CXX_FLAGS:STRING=-I${includeDir}`);
      flags.push('-DCMAKE_C_FLAGS:STRING=-DLWIP_RAW=1 -DLWIP_SO_RCVTIMEO=1');
    }
    run('cmake', flags);
    run('cmake', ['--build', cmakeBuild, '--config', 'Release', '--target', 'zt-static', '--parallel', '4']);
    const source = findLibrary(join(cmakeBuild, 'lib')) ?? findLibrary(join(cmakeBuild, 'bin'));
    if (!source) throw new Error(`Static library not found under ${cmakeBuild}`);
    mkdirSync(nativeDir, { recursive: true });
    copyFileSync(source, nativeFile);
    writeFileSync(nativeBuildStamp, nativeBuildRevision);
    console.log(`Static library: ${source} -> ${nativeFile}`);
  }
  console.log(`Embedded libzt static library ready: ${nativeFile}`);
} catch (err) {
  console.error('\nEmbedded libzt preparation failed:', err.message);
  process.exit(1);
}
