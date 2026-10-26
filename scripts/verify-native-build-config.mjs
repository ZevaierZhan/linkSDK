import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {libztBuildPlan, nativeBuildInputs, windowsNinjaEnv, windowsMsvcEnv} from './libzt-build-config.mjs';

const root = path.resolve(import.meta.dirname, '..');
const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'link-build-inputs-'));
let count = 0;
try {
  for (const input of nativeBuildInputs) {
    const p = path.join(fixture, input); fs.mkdirSync(path.dirname(p), {recursive: true}); fs.writeFileSync(p, fs.readFileSync(path.join(root, input)));
  }
  const before = libztBuildPlan(fixture, 'win32', 'x64');
  assert.equal(before.generator, 'Ninja');
  assert.notEqual(before.directory, 'planet-node-native-release');
  assert.deepEqual(before, libztBuildPlan(fixture, 'win32', 'x64')); count++;
  assert.notEqual(before.fingerprint, libztBuildPlan(fixture, 'win32', 'arm64').fingerprint); count++;
  fs.appendFileSync(path.join(fixture, 'scripts/diagnostics/actual-socket-stun.hpp'), '\n// simulated source edit\n');
  assert.notEqual(before.fingerprint, libztBuildPlan(fixture, 'win32', 'x64').fingerprint); count++;
  assert.equal(libztBuildPlan(root, 'darwin', 'arm64').generator, null); count++;
  const env = windowsNinjaEnv({VSINSTALLDIR: 'C:/VS', PATH: 'old', CMAKE_GENERATOR: 'NMake Makefiles', CC: 'gcc', CXX: 'g++'}, () => true);
  assert.equal(env.CMAKE_GENERATOR, 'Ninja'); assert.equal(env.CC, 'cl'); assert.equal(env.CXX, 'cl'); assert.ok(env.PATH.endsWith(';old')); count++;
  for (const spelling of ['path','Path']) {
    const inherited=windowsNinjaEnv({VSINSTALLDIR:'C:/VS',[spelling]:'C:/CMake/bin;C:/Windows/System32'},()=>true);
    assert.deepEqual(Object.keys(inherited).filter(key=>key.toLowerCase()==='path'),['PATH']);
    assert.ok(inherited.PATH.endsWith(';C:/CMake/bin;C:/Windows/System32'));count++;
  }
  const calls = [];
  const regular = windowsMsvcEnv({PATH: 'old'}, {exists: () => true, run: (command, args, childEnv) => {
    calls.push({command, args, childEnv});
    return command.endsWith('vswhere.exe') ? 'C:/VS\r\n' : 'Path=C:/VS/bin\r\nVSINSTALLDIR=C:/VS\r\nINCLUDE=headers\r\nLIB=libraries\r\nVSCMD_VER=test\r\n';
  }});
  assert.equal(regular.INCLUDE, 'headers'); assert.equal(regular.LIB, 'libraries'); assert.equal(calls.length, 2);
  assert.equal(regular.PATH, undefined); assert.equal(regular.Path, 'C:/VS/bin');
  assert.equal(calls[1].args.at(-1), 'call "%LINK_BUILD_VCVARS%" >nul && set'); assert.equal(regular.LINK_BUILD_VCVARS, undefined); count++;
  assert.equal(windowsMsvcEnv(regular, {run: () => {throw Error('Already initialized');}}).LIB, 'libraries'); count++;
  if (process.platform === 'win32') {
    const uninitialized = Object.fromEntries(Object.entries(process.env).filter(([key]) => !['include', 'lib', 'libpath', 'vscmd_ver'].includes(key.toLowerCase())));
    const ready = windowsMsvcEnv(uninitialized);
    assert.ok(ready.INCLUDE && ready.LIB); count++;
    const spaced = path.join(fixture, 'VS tools'), batch = path.join(spaced, 'VC/Auxiliary/Build/vcvars64.bat');
    fs.mkdirSync(path.dirname(batch), {recursive: true});
    fs.writeFileSync(batch, '@set "VSINSTALLDIR="\r\n@call "%LINK_TEST_VCVARS%"\r\n');
    const fromSpacedPath = windowsMsvcEnv({...uninitialized, VSINSTALLDIR: spaced, LINK_TEST_VCVARS: path.join(ready.VSINSTALLDIR, 'VC/Auxiliary/Build/vcvars64.bat')});
    assert.ok(fromSpacedPath.INCLUDE && fromSpacedPath.LIB); count++;
  }
  console.log(`PASS ${count} native build configuration checks`);
} finally {fs.rmSync(fixture, {recursive: true, force: true});}
