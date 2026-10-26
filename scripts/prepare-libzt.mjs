import {libztBuildPlan, windowsNinjaEnv, windowsMsvcEnv} from './libzt-build-config.mjs';
import{createHash}from'node:crypto';
import {patchLibzt} from "./patch-libzt.mjs";
import { existsSync, mkdirSync, copyFileSync, readdirSync, statSync, lstatSync, realpathSync, readlinkSync, readFileSync, writeFileSync } from 'node:fs';
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

// Pristine git-archive inputs, never a patched worktree or a native build cache.
// The child commits are the gitlinks in the pinned libzt tree.
const sourcePins = [
  ['libzt', sdkRevision, '', '36d1d04f2305e47f72d2b26e62374a8c222840b86223f40df012812667840ba6', 'pkg/crate/libzt/Cargo.toml'],
  ['zerotier', 'c53c6bd9c320ea839c525972d8afba448a58606e', 'ext/ZeroTierOne', '355b55c00ef1c25d9d709366862ff3dacc4947f09259b2880bd274ee503377c9', 'CMakeLists.txt'],
  ['lwip', '32708c0a8b140efb545cc35101ee5fdeca6d6489', 'ext/lwip', 'f916a042ffc5d594cfce7ce94186d49a584d04faf78a9f67261795a13539e1c3', 'src/core/init.c'],
  ['lwip-contrib', '4fd612c9c72dfcd1db6618bd59c1a17d9f5b55f8', 'ext/lwip-contrib', 'ecc3b4b4ded4420bb634a088852f30e32784fa25333cf8f44c88e366c6123d09', 'apps/httpserver/httpserver-netconn.c'],
];
// Official v3.11.3 tag -> commit, then exact GitHub content blobs (not a
// vendor-directory rev-parse, which could resolve the application's parent).
const jsonCommit = '9cca280a4d0ccf0c08f47a99aa71d1b0e52f8d03';
const jsonPins = [
  [`json-${jsonCommit}.hpp`, 'single_include/nlohmann/json.hpp', '9bea4c8066ef4a1c206b2be5a36302f8926f7fdc6087af5d20b417d0cf103ea6', '8b72ea6539f40e4e3f7762d17e30c67f2a8308ed'],
  [`json-${jsonCommit}-LICENSE.MIT`, 'LICENSE.MIT', '86b998c792894ccb911a1cb7994f7a9652894e7a094c0b5e45be2f553f45cf14', '1c1f7a690d815db3a79ea3d4c9138a497e2e7702'],
];
const sourceStamp = JSON.stringify({source:'verified pristine git archive', pins:sourcePins, nlohmann:{version:'v3.11.3',commit:jsonCommit,files:jsonPins}});
const stampFile = join(vend, '.link-pinned-source.json');
function inspectSourceTar(bytes) {
  const links = new Set(), names = new Set(), entries = [];
  const text = (header, start, length) => header.subarray(start, start + length).toString('utf8').split('\0')[0];
  for (let offset = 0, count = 0; offset + 512 <= bytes.length;) {
    const header = bytes.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    if (++count > 10000) throw Error('Source archive contains too many entries');
    const sizeText = text(header, 124, 12).trim();
    if (!/^[0-7]+$/.test(sizeText)) throw Error('Invalid source archive size');
    const size = parseInt(sizeText, 8), type = text(header, 156, 1) || '0';
    if (size > 64 * 1024 * 1024 || offset + 512 + size > bytes.length) throw Error('Source archive entry exceeds bounds');
    if (type === 'g') {
      // git archive emits only its commit comment as a global PAX header.
      if (!/^\d+ comment=[a-f0-9]{40}\n$/.test(bytes.subarray(offset + 512, offset + 512 + size).toString())) throw Error('Unexpected source archive metadata');
    } else {
      const prefix = text(header, 345, 155), leaf = text(header, 0, 100);
      const name = (prefix ? prefix + '/' : '') + leaf;
      const clean = name.replace(/\/$/, '');
      if (!clean || clean.startsWith('/') || clean.includes('\\') || /[\r\n\0]/.test(clean)
          || clean.split('/').some(part => !part || part === '.' || part === '..' || part === '.git')
          || names.has(clean) || [...links].some(link => clean.startsWith(link + '/'))
          || !['0','5','2'].includes(type)) throw Error('Unsafe source archive entry');
      names.add(clean);
      entries.push({name:clean, type});
      if (type === '2') {
        // These are upstream's tracked Linux library aliases, unused by iOS.
        if (!/^ext\/libpqxx-7\.7\.3\/install\/ubuntu22\.04\/(amd64|arm64)\/lib\/libpqxx\.a$/.test(clean)
            || text(header, 157, 100) !== 'libpqxx-7.7.a') throw Error('Unexpected source archive link');
        links.add(clean);
      }
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return entries;
}
function cachedIOSSource() {
  if (!process.argv.includes('--ios') || !process.env.LINK_IOS_LIBZT_SOURCE_CACHE) return false;
  const cache = resolve(process.env.LINK_IOS_LIBZT_SOURCE_CACHE);
  const directory = lstatSync(cache);
  if (!directory.isDirectory() || directory.isSymbolicLink() || realpathSync(cache) !== cache
      || directory.uid !== process.getuid() || (directory.mode & 0o077)) throw Error('libzt source cache must be a private owned directory');
  const archives = sourcePins.map(([name, commit]) => join(cache, `${name}-${commit}.tar`));
  const jsonFiles = jsonPins.map(([name]) => join(cache, name));
  const inputs = [...archives, ...jsonFiles];
  const present = inputs.filter(file => {
    try { lstatSync(file); return true; } catch (error) { if (error.code === 'ENOENT') return false; throw error; }
  }).length;
  if (!present) {
    if (existsSync(stampFile)) throw Error('Pinned libzt source copy has lost its cache inputs');
    return false; // An empty cache preserves the ordinary Git path.
  }
  if (present !== inputs.length) throw Error('Incomplete pinned libzt source cache');
  const manifests = [];
  for (let i = 0; i < inputs.length; i++) {
    const file = inputs[i], stat = lstatSync(file);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.nlink !== 1 || stat.uid !== process.getuid()
        || (stat.mode & 0o077) || stat.size > 80 * 1024 * 1024) throw Error('Unsafe libzt source archive');
    const bytes = readFileSync(file);
    const expected = i < archives.length ? sourcePins[i][3] : jsonPins[i - archives.length][2];
    if (createHash('sha256').update(bytes).digest('hex') !== expected) throw Error('Pinned libzt source archive checksum mismatch');
    if (i < archives.length) manifests.push(inspectSourceTar(bytes));
  }
  const complete = () => sourcePins.every(([, , subdirectory, , sentinel], i) =>
    existsSync(join(vend, subdirectory, sentinel)) && manifests[i].every(entry => {
      const file = join(vend, subdirectory, entry.name);
      try {
        const stat = lstatSync(file);
        return entry.type === '5' ? stat.isDirectory() && !stat.isSymbolicLink()
          : entry.type === '2' ? stat.isSymbolicLink() && readlinkSync(file) === 'libpqxx-7.7.a'
          : stat.isFile() && !stat.isSymbolicLink() && stat.nlink === 1;
      } catch (error) { if (error.code === 'ENOENT') return false; throw error; }
    })) && realpathSync(jsonDir) === jsonDir && jsonPins.every(([, name, sha]) => {
      const file = join(jsonDir, name);
      if (!existsSync(file)) return false;
      const stat = lstatSync(file);
      return stat.isFile() && !stat.isSymbolicLink() && stat.nlink === 1 && realpathSync(file) === file
        && createHash('sha256').update(readFileSync(file)).digest('hex') === sha;
    });
  if (existsSync(vend)) {
    if (lstatSync(vend).isSymbolicLink() || realpathSync(vend) !== vend
        || !existsSync(stampFile) || lstatSync(stampFile).isSymbolicLink()
        || readFileSync(stampFile, 'utf8') !== sourceStamp || !complete()) throw Error('Existing libzt tree is not a complete pinned source-cache copy');
  } else {
    if (existsSync(jsonDir)) throw Error('Refusing to overwrite an existing nlohmann source directory');
    mkdirSync(join(root, 'vendor'), {recursive:true});
    if (realpathSync(join(root, 'vendor')) !== join(root, 'vendor')) throw Error('Source vendor directory is not owned by this checkout');
    mkdirSync(vend, {recursive:true});
    for (let i = 0; i < archives.length; i++) {
      const destination = join(vend, sourcePins[i][2]);
      mkdirSync(destination, {recursive:true});
      run('/usr/bin/tar', ['-xf', archives[i], '--no-same-owner', '-C', destination]);
    }
    mkdirSync(join(jsonDir, 'single_include/nlohmann'), {recursive:true});
    for (let i = 0; i < jsonFiles.length; i++) copyFileSync(jsonFiles[i], join(jsonDir, jsonPins[i][1]));
    if (!complete()) throw Error('Pinned libzt source extraction is incomplete');
    writeFileSync(stampFile, sourceStamp, {flag:'wx', mode:0o600});
  }
  console.log(JSON.stringify({libztSource:'verified pristine git archive cache',revision:sdkRevision,archiveSha256:sourcePins.map(pin=>pin[3]),nlohmann:{version:'v3.11.3',commit:jsonCommit,files:jsonPins}}));
  return true;
}

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
  const cached = cachedIOSSource();
  if (!cached && !existsSync(manifest)) {
    mkdirSync(join(root, 'vendor'), { recursive: true });
    run('git', ['clone', '--no-checkout', '--depth', '1', 'https://github.com/zerotier/libzt.git', vend]);
    run('git', ['fetch', '--depth', '1', 'origin', sdkRevision], vend);
    run('git', ['checkout', '--detach', sdkRevision], vend);
    run('git', ['submodule', 'update', '--init', '--recursive'], vend);
  }
  if (!cached && (!existsSync(join(vend, 'ext', 'ZeroTierOne', 'CMakeLists.txt')) ||
      !existsSync(join(vend, 'ext', 'lwip', 'src')))) {
    run('git', ['submodule', 'update', '--init', '--recursive'], vend);
  }
  const actualRevision = cached ? sdkRevision : execFileSync('git', ['rev-parse', 'HEAD'], {cwd:vend,encoding:'utf8'}).trim();
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
  if (process.argv.includes('--android') || process.argv.includes('--ios')) {
    console.log('libzt sources ready for ' + (process.argv.includes('--ios') ? 'iOS' : 'Android') + '; native build is target-specific.');
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
