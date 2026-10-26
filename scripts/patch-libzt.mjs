import {patchLibztPacketAdapter} from './patch-libzt-packet-adapter.mjs';
import {readFileSync as readSource, writeFileSync, existsSync} from 'node:fs';
import {join} from 'node:path';
// Git for Windows may check sources out as CRLF; patch anchors are LF.
function readFileSync(path,encoding){const v=readSource(path,encoding);return typeof v==='string'?v.replaceAll('\r\n','\n'):v;}
export function patchLibzt(vend) {
  // Keep errno snapshots in the SDK itself: OS errno and the public legacy
  // zts_errno have different platform/thread semantics. The global ABI remains.
  modifyNative('src/Sockets.cpp', [[
    'int zts_errno;',
    'int zts_errno;\nstatic thread_local int link_socket_error = 0;',
  ], [
    'int zts_bsd_socket(const int socket_family, const int socket_type, const int protocol)',
    `int ZTCALL zts_get_thread_socket_error(void)
{
    return link_socket_error;
}

void zts_record_socket_error(int error)
{
    link_socket_error = error;
}

int zts_bsd_socket(const int socket_family, const int socket_type, const int protocol)`,
  ]]);
  modifyNative('include/ZeroTierSockets.h', [[
    'ZTS_API int ZTCALL zts_bsd_socket(int family, int type, int protocol);',
    `/** Link: same-thread socket error snapshot. Read only after ZTS_ERR_SOCKET.
 * BSD wrapper entry clears the snapshot; SERVICE/ARG statuses are not errno. */
ZTS_API int ZTCALL zts_get_thread_socket_error(void);

ZTS_API int ZTCALL zts_bsd_socket(int family, int type, int protocol);`,
  ]]);
  modifyNative('pkg/crate/libzt/src/include/ZeroTierSockets.h', [[
    'ZTS_API int ZTCALL zts_bsd_socket(int family, int type, int protocol);',
    `/** Link: same-thread socket error snapshot. Read only after ZTS_ERR_SOCKET.
 * BSD wrapper entry clears the snapshot; SERVICE/ARG statuses are not errno. */
ZTS_API int ZTCALL zts_get_thread_socket_error(void);

ZTS_API int ZTCALL zts_bsd_socket(int family, int type, int protocol);`,
  ]]);
  // Hook set_errno itself: get_socket(EBADF), select and other early paths do
  // not all use sock_set_errno. Preserve its original nonzero OS-errno writes.
  modifyNative('ext/lwip/src/include/lwip/priv/sockets_priv.h', [[
    '#define set_errno(err) do { if (err) { errno = (err); } } while(0)',
    `#ifdef __cplusplus
extern "C" void zts_record_socket_error(int error);
#else
extern void zts_record_socket_error(int error);
#endif
#define set_errno(err) do { const int link_error = (err); if (link_error) { errno = link_error; } zts_record_socket_error(link_error); } while(0)`,
  ]]);
  const socketSourcePath = join(vend, 'src/Sockets.cpp');
  let socketSource = readFileSync(socketSourcePath, 'utf8');
  const entryMarker = '// LINK_THREAD_LOCAL_SOCKET_ERROR_ENTRY';
  if (!socketSource.includes(entryMarker)) {
    let entries = 0;
    socketSource = socketSource.replace(/(\b(?:int|ssize_t)\s+zts_bsd_\w+\([^)]*\)\s*\{\n)/g, signature => {
      entries++;
      return signature + '    zts_record_socket_error(0); ' + entryMarker + '\n';
    });
    if (entries !== 25) throw new Error(`Pinned SDK socket entry layout changed: ${entries}`);
    writeFileSync(socketSourcePath, socketSource);
  } else if (socketSource.split(entryMarker).length - 1 !== 25) {
    throw new Error('Incomplete SDK thread-local socket entry patch');
  }
  // Publish the callback loop's running flag before a newly scheduled child can
  // inspect it. Otherwise the child exits immediately and ready events are lost.
  if (!readFileSync(join(vend,'src/Controls.cpp'),'utf8').includes('LINK_EXTERNAL_CALLBACK_LIFETIME')) {
  modifyNative('src/Controls.cpp', [[
    `    if (zts_events->hasCallback()) {
#if defined(__WINDOWS__)
        HANDLE callbackThread = CreateThread(NULL, 0, cbRun, NULL, 0, NULL);
        // TODO: Check success
#else
        pthread_t cbThread;
        if ((res = pthread_create(&cbThread, NULL, cbRun, NULL)) != 0) {}
#endif
#if defined(__linux__)
        // pthread_setname_np(cbThread, ZTS_EVENT_CALLBACK_THREAD_NAME);
#endif
        if (res != ZTS_ERR_OK) {
            zts_events->clrState(ZTS_STATE_CALLBACKS_RUNNING);
            zts_events->clrCallback();
        }
        zts_events->setState(ZTS_STATE_CALLBACKS_RUNNING);
    }`,
    `    if (zts_events->hasCallback()) {
        zts_events->setState(ZTS_STATE_CALLBACKS_RUNNING);
#if defined(__WINDOWS__)
        HANDLE callbackThread = CreateThread(NULL, 0, cbRun, NULL, 0, NULL);
        res = callbackThread ? ZTS_ERR_OK : ZTS_ERR_SERVICE;
        if (callbackThread) CloseHandle(callbackThread);
#else
        pthread_t cbThread;
        res = pthread_create(&cbThread, NULL, cbRun, NULL);
#endif
        if (res != ZTS_ERR_OK) {
            zts_events->clrState(ZTS_STATE_CALLBACKS_RUNNING);
            zts_events->clrCallback();
            return ZTS_ERR_SERVICE;
        }
    }`,
  ]]);
  }
  // Local opt-in diagnostics reuse the existing validated UDP descriptor.
  writeFileSync(join(vend, 'src', 'LinkSocketStun.hpp'), readFileSync(new URL('./diagnostics/actual-socket-stun.hpp', import.meta.url), 'utf8'));
  modifyNative('src/NodeService.cpp', [
    ['#include "Events.hpp"', '#include "Events.hpp"\n#include "LinkSocketStun.hpp"'],
    ['    if (_forceTcpRelay) {\n        return;\n    }\n    ZTS_UNUSED_ARG(uptr);',
     '    if (_forceTcpRelay) {\n        return;\n    }\n    if (LinkSocketStun::consume(_phy.getDescriptor(sock), localAddr, from, data, len)) return;\n    ZTS_UNUSED_ARG(uptr);'],
    ['        return ((r) ? 0 : -1);',
     '        if (r) LinkSocketStun::maybeSend(_phy.getDescriptor((PhySocket*)((uintptr_t)localSocket)), addr);\n        return ((r) ? 0 : -1);'],
  ]);
  // A virtual TAP has no OS multicast subscription list. Register ordinary
  // Ethernet broadcast explicitly so a Root's gather reply includes itself.
  // ARP's address-specific ADI subscriptions do not cover UDP broadcast.
  modifyNative('src/VirtualTap.cpp', [[
    '    // TODO: get multicast subscriptions\n',
    '    // Ordinary Ethernet broadcast (ADI 0), distinct from ARP subscriptions.\n    newGroups.push_back(MulticastGroup(MAC(0xffffffffffffULL), 0));\n    // TODO: get multicast subscriptions\n',
  ]]);
  // Keep storage alive through the official Node/Topology destructor, which saves peers.
  modifyNative('src/NodeService.cpp', [[
    `    _nodeId = 0x0;
    _primaryPort = 0;
    _homePath.clear();
    _allowNetworkCaching = true;
    _allowPeerCaching = true;
    _allowIdentityCaching = true;
    _allowRootSetCaching = true;
    memset(_publicIdStr, 0, ZT_IDENTITY_STRING_BUFFER_LENGTH);
    memset(_secretIdStr, 0, ZT_IDENTITY_STRING_BUFFER_LENGTH);
    _interfacePrefixBlacklist.clear();`,
    `    // Do not reset storage/caching before Node destruction flushes peer state.
    // The service thread deletes this instance after run() finishes.`,
  ]]);
  modifyNative('src/Controls.cpp', [[
    'int zts_node_stop()',
    `int link_node_wait_stopped(unsigned int timeout_ms)
{
    for (unsigned int elapsed = 0; elapsed < timeout_ms; elapsed += 10) {
        { Mutex::Lock lock(service_m); if (!zts_service) return ZTS_ERR_OK; }
        zts_util_delay(10);
    }
    return ZTS_ERR_SERVICE;
}

int zts_node_stop()`,
  ]]);
  const crate = join(vend,'pkg','crate','libzt');
  function modify(file, replacements) {
    const p=join(crate,file);
    if (!existsSync(p)) throw new Error(`Missing SDK source: ${p}`);
    let body=readFileSync(p,'utf8'); let changed=false;
    for (const [oldText,newText] of replacements) {
      if(file.endsWith('ZeroTierSockets.h')&&oldText==='ZTS_API int ZTCALL zts_init_enable_controller(int enabled);'&&body.includes('ZTS_API int ZTCALL zts_controller_create_network(')&&body.includes('ZTS_API int ZTCALL zts_controller_authorize_member('))continue;
      if(body.includes(newText)) continue;
      if(!body.includes(oldText)) throw new Error(`SDK layout changed in ${file}: ${oldText.slice(0,70)}`);
      body=body.replace(oldText,newText);changed=true;
    }
    if(changed) writeFileSync(p,body);
  }
  function modifyNative(file, replacements) {
    const p = join(vend, file);
    if (!existsSync(p)) throw new Error(`Missing native SDK source: ${p}`);
    let body = readFileSync(p, 'utf8');
    let changed = false;
    const dedupeFunction = (source, signature) => {
      let first = source.indexOf(signature);
      if (first < 0) return source;
      const blockEnd = (start) => {
        const open = source.indexOf('{', start);
        if (open < 0) return -1;
        let depth = 0;
        for (let i = open; i < source.length; i++) {
          if (source[i] === '{') depth++;
          else if (source[i] === '}' && --depth === 0) return i + 1;
        }
        return -1;
      };
      let end = blockEnd(first);
      while (end >= 0) {
        const next = source.indexOf(signature, end);
        if (next < 0) break;
        const nextEnd = blockEnd(next);
        if (nextEnd < 0) break;
        source = source.slice(0, next) + source.slice(nextEnd);
        end = blockEnd(first);
      }
      return source;
    };
    for (const signature of file === 'src/NodeService.cpp'
      ? ['int NodeService::createControllerNetwork(', 'int NodeService::authorizeControllerMember(']
      : file === 'src/Controls.cpp'
        ? ['int zts_init_enable_controller(', 'int zts_controller_create_network(', 'int zts_controller_authorize_member(']
        : []) {
      const normalized = dedupeFunction(body, signature);
      if (normalized !== body) { body = normalized; changed = true; }
    }
    for (const [oldText, newText] of replacements) {
      if(file.endsWith('ZeroTierSockets.h')&&oldText==='ZTS_API int ZTCALL zts_init_enable_controller(int enabled);'&&body.includes('ZTS_API int ZTCALL zts_controller_create_network(')&&body.includes('ZTS_API int ZTCALL zts_controller_authorize_member('))continue;
      if (file === 'src/Controls.cpp' && oldText === 'int zts_init_set_roots(const void* roots_data, unsigned int len)' &&
          body.includes('int zts_init_enable_controller(')) continue;
      if (file === 'src/NodeService.cpp' && oldText === 'int NodeService::enableEmbeddedController(bool enabled)' && newText === oldText) {
        const anchor = oldText;
        const methods = [
          [`int NodeService::createControllerNetwork(`, `int NodeService::createControllerNetwork(const char* name, uint64_t* networkId)
{
    if (!_embeddedController || !name || !networkId) return ZTS_ERR_SERVICE;
    *networkId = _embeddedController->createLinkNetwork(name);
    return *networkId ? ZTS_ERR_OK : ZTS_ERR_GENERAL;
}

`],
          [`int NodeService::authorizeControllerMember(`, `int NodeService::authorizeControllerMember(uint64_t networkId, uint64_t memberId)
{
    if (!_embeddedController) return ZTS_ERR_SERVICE;
    return _embeddedController->authorizeLinkMember(networkId, memberId) ? ZTS_ERR_OK : ZTS_ERR_GENERAL;
}

`],
          [`int NodeService::listControllerMembers(`, `int NodeService::listControllerMembers(uint64_t networkId, char* out, unsigned int* outLen)
{
    if (!_embeddedController || !outLen) return ZTS_ERR_SERVICE;
    std::string json;
    if (!_embeddedController->listLinkMembers(networkId, json)) return ZTS_ERR_GENERAL;
    const unsigned int required = static_cast<unsigned int>(json.size() + 1);
    if (!out) { *outLen = required; return ZTS_ERR_OK; }
    if (*outLen < required) { *outLen = required; return ZTS_ERR_ARG; }
    memcpy(out, json.c_str(), required);
    *outLen = required;
    return ZTS_ERR_OK;
}

`],
        ];
        for (const [signature, method] of methods) {
          if (!body.includes(signature)) {
            const index = body.indexOf(anchor);
            if (index < 0) throw new Error(`SDK layout changed in ${file}: ${anchor}`);
            body = body.slice(0, index) + method + body.slice(index);
            changed = true;
          }
        }
        continue;
      }
      if (file === 'ext/ZeroTierOne/controller/EmbeddedNetworkController.cpp' &&
          oldText.includes('configureHTTPControlPlane(') &&
          body.includes('uint64_t EmbeddedNetworkController::createLinkNetwork(')) {
        const start = body.indexOf('uint64_t EmbeddedNetworkController::createLinkNetwork(');
        const end = body.indexOf('void EmbeddedNetworkController::configureHTTPControlPlane(', start);
        const networkFactory = body.slice(start, end);
        if (!networkFactory.includes('{"target", "10.247.0.0/24"')) {
          const pool = '{"ipAssignmentPools", {{{"ipRangeStart", "10.247.0.1"}, {"ipRangeEnd", "10.247.0.254"}}}}';
          if (!networkFactory.includes(pool)) throw new Error('Controller network factory changed before managed route insertion');
          body = body.slice(0, start) + networkFactory.replace(pool, `${pool},\n            {"routes", {{{"target", "10.247.0.0/24"}, {"via", nullptr}}}}`) + body.slice(end);
          changed = true;
        }
        if (!body.includes('bool EmbeddedNetworkController::listLinkMembers(')) {
          const method = `bool EmbeddedNetworkController::listLinkMembers(uint64_t networkId, std::string &membersJson)
{
    json network, output = json::array();
    std::vector<json> members;
    if (!_db.get(networkId, network, members)) return false;
    for (const auto &member : members) {
        json row = json::object();
        for (const char *key : {"id", "identity", "name", "authorized", "ipAssignments", "lastSeen", "revision"}) {
            if (member.contains(key)) row[key] = member[key];
        }
        output.push_back(row);
    }
    membersJson = output.dump();
    return true;
}

`;
          const insertAt = body.indexOf('uint64_t EmbeddedNetworkController::createLinkNetwork(');
          body = body.slice(0, insertAt) + method + body.slice(insertAt);
          changed = true;
        }
        const oldAuthorize = `bool EmbeddedNetworkController::authorizeLinkMember(uint64_t networkId, uint64_t memberId)
{
    json network, member;
    if (!_db.get(networkId, network)) return false;
    _db.get(networkId, network, memberId, member);
    DB::initMember(member);
    char networkHex[17], memberHex[11];
    snprintf(networkHex, sizeof(networkHex), "%016llx", (unsigned long long)networkId);
    snprintf(memberHex, sizeof(memberHex), "%010llx", (unsigned long long)memberId);
    member["id"] = memberHex;
    member["address"] = memberHex;
    member["nwid"] = networkHex;
    if (!OSUtils::jsonBool(member["authorized"], false)) {
        member["authorized"] = true;
        member["lastAuthorizedTime"] = OSUtils::now();
        member["lastAuthorizedCredentialType"] = "link-invite";
        member["lastAuthorizedCredential"] = json();
    }
    DB::cleanMember(member);
    return _db.save(member, true);
}`;
        const newAuthorize = `bool EmbeddedNetworkController::authorizeLinkMember(uint64_t networkId, uint64_t memberId)
{
    json network, member;
    if (!_db.get(networkId, network)) return false;
    if (_db.get(networkId, network, memberId, member) && OSUtils::jsonBool(member["authorized"], false)) {
        return true;
    }
    DB::initMember(member);
    char networkHex[17], memberHex[11];
    snprintf(networkHex, sizeof(networkHex), "%016llx", (unsigned long long)networkId);
    snprintf(memberHex, sizeof(memberHex), "%010llx", (unsigned long long)memberId);
    member["id"] = memberHex;
    member["address"] = memberHex;
    member["nwid"] = networkHex;
    if (!OSUtils::jsonBool(member["authorized"], false)) {
        member["authorized"] = true;
        member["lastAuthorizedTime"] = OSUtils::now();
        member["lastAuthorizedCredentialType"] = "link-invite";
        member["lastAuthorizedCredential"] = json();
    }
    DB::cleanMember(member);
    if (_db.save(member, true)) return true;
    // FileDB::save returns false both when persistence fails and when the
    // requested record is already identical. Authorization is idempotent, so
    // verify the stored state before reporting failure to the C API.
    json savedNetwork, savedMember;
    return _db.get(networkId, savedNetwork, memberId, savedMember) &&
        OSUtils::jsonBool(savedMember["authorized"], false);
}`;
        const sectionStart = body.indexOf('uint64_t EmbeddedNetworkController::createLinkNetwork(');
        const sectionEnd = body.indexOf('void EmbeddedNetworkController::configureHTTPControlPlane(', sectionStart);
        const patchedFactory = body.slice(sectionStart, sectionEnd);
        if (!patchedFactory.replace(/\s+/g,' ').includes(newAuthorize.replace(/\s+/g,' '))) {
          if (!patchedFactory.includes(oldAuthorize)) throw new Error('SDK layout changed before idempotent member authorization patch');
          body = body.replace(oldAuthorize, newAuthorize);
          changed = true;
        }
        continue;
      }
      if (body.includes(newText)) continue;
      if (!body.includes(oldText)) throw new Error(`SDK layout changed in ${file}: ${oldText.slice(0, 70)}`);
      body = body.replace(oldText, newText);
      changed = true;
    }
    if (changed) writeFileSync(p, body);
  }
  // Accept both pristine upstream SDK and an existing manual Windows patch.
  // Normalize once to a cross-platform setting; subsequent runs are no-ops.
  const buildFile = join(crate, 'build.rs');
  let buildSource = readFileSync(buildFile, 'utf8');
  const sizeSetting = '.size_t_is_usize(!cfg!(target_os = "windows"))';
  if (!buildSource.includes(sizeSetting)) {
    if (buildSource.includes('.size_t_is_usize(false)')) {
      buildSource = buildSource.replace('.size_t_is_usize(false)', sizeSetting);
    } else if (buildSource.includes('.size_t_is_usize(true)')) {
      buildSource = buildSource.replace('.size_t_is_usize(true)', sizeSetting);
    } else {
      throw new Error('SDK build.rs has an unknown size_t_is_usize configuration');
    }
    writeFileSync(buildFile, buildSource);
  }
  modify('build.rs', [
    ['println!("cargo:rustc-link-lib=static=zt");',
     `println!("cargo:rustc-link-lib=static=zt");
    println!("cargo:rerun-if-changed={}", lib_dir.join(if cfg!(target_os = "windows") { "zt.lib" } else { "libzt.a" }).display());`],
    ['.size_t_is_usize(true)', sizeSetting],
    ['println!("cargo:rustc-link-lib=dylib=c++");',
     `if cfg!(target_os = "macos") { println!("cargo:rustc-link-lib=dylib=c++"); }
    else if cfg!(target_os = "linux") { println!("cargo:rustc-link-lib=dylib=stdc++"); }
    else if cfg!(target_os = "windows") { println!("cargo:rustc-link-lib=Iphlpapi"); }`],
  ]);
  // Cargo build scripts execute on the host. Mobile SDK dispatch uses Cargo's target.
  let mobileBuild = readFileSync(buildFile, 'utf8');
  const helperStart = mobileBuild.search(/\nfn build_(?:android|ios)\(/);
  if (helperStart >= 0) mobileBuild = mobileBuild.slice(0, helperStart);
  for (const target of ['android', 'ios']) {
    const dispatch = 'if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("' + target + '") { build_' + target + '(); return; }';
    if (!mobileBuild.includes(dispatch)) mobileBuild = mobileBuild.replace('fn main() {', 'fn main() {\n    ' + dispatch);
  }
  writeFileSync(buildFile, mobileBuild + '\n' + readFileSync(new URL('./libzt-android-build.rs', import.meta.url), 'utf8') + '\n' + readFileSync(new URL('./libzt-ios-build.rs', import.meta.url), 'utf8'));
  // zts_util_delay may already have been fixed manually on Windows.
  // Do not overwrite an existing conversion or falsely report an SDK layout change.
  const nodeFile = join(crate, 'src', 'node.rs');
  let nodeSource = readFileSync(nodeFile, 'utf8');
  if (nodeSource.includes('zts_util_delay(interval_ms)')) {
    nodeSource = nodeSource.replace('zts_util_delay(interval_ms)', 'zts_util_delay(interval_ms as _)');
    writeFileSync(nodeFile, nodeSource);
  } else if (!/zts_util_delay\(\s*(?:interval_ms\s+as\s+_|interval_ms\.min\(u32::MAX as u64\)\s+as\s+u32|interval_ms\.try_into\(\)\.unwrap_or\(u32::MAX\))\s*\)/.test(nodeSource)) {
    throw new Error('Unrecognized zts_util_delay call in src/node.rs; refusing to overwrite it');
  }
  // C char is unsigned on Android ARM64 and signed on x86_64.
  modify('src/node.rs', [[
    'let mut v = vec![0; (ZTS_INET6_ADDRSTRLEN as usize) + 1];',
    'let mut v = vec![0 as std::os::raw::c_char; (ZTS_INET6_ADDRSTRLEN as usize) + 1];'
  ], ['let ptr = v.as_mut_ptr() as *mut i8;', 'let ptr = v.as_mut_ptr();']]);
  modify('src/socket.rs', [['tv_sec: secs,','tv_sec: secs as _,']]);
  modify('src/tcp.rs', [['buf.as_ptr() as *const c_void, len)','buf.as_ptr() as *const c_void, len as _)']]);
  modify('src/utils.rs', [
    ['(addr.s_addr as u32).to_be()',
     '{ #[cfg(windows)] { (addr.S_addr as u32).to_be() } #[cfg(not(windows))] { (addr.s_addr as u32).to_be() } }'],
  ]);
  // Compile the official file-backed controller into the same static libzt
  // library. Keep the patch here because vendor/ is generated by prepare-libzt.
  const nativeBuild = join(vend, 'CMakeLists.txt');
  let cmake = readFileSync(nativeBuild, 'utf8');
  // Android uses the Rust C API, not libzt's Java/JNI wrapper.
  const androidBlock = cmake.indexOf('# Android AAR containing libzt.so');
  const androidEnd = cmake.indexOf('\nif(BUILD_MACOS_FRAMEWORK)', androidBlock);
  if(androidBlock<0||androidEnd<0)throw Error('Missing Android CMake block');
  cmake=cmake.slice(0,androidBlock)+`# Android AAR containing libzt.so
if(CMAKE_SYSTEM_NAME STREQUAL "Android")
    set(BUILD_ANDROID TRUE)
    set(BUILD_STATIC_LIB TRUE)
    set(BUILD_SHARED_LIB FALSE)
    set(BUILD_HOST_SELFTEST FALSE)
    set(BUILD_HOST_EXAMPLES FALSE)
    set(ALLOW_INSTALL_TARGET FALSE)
    set(ZTS_NDK_ONLY TRUE)
endif()
`+cmake.slice(androidEnd);
  const controllerIncludes = 'include_directories(${ZTO_SRC_DIR}/controller)\ninclude_directories(${ZTO_SRC_DIR}/ext)\ninclude_directories(${PROJ_DIR}/../nlohmann-json/single_include)';
  if (!cmake.includes(controllerIncludes)) {
    const anchor = 'include_directories(${ZTO_SRC_DIR}/node)';
    if (!cmake.includes(anchor)) throw new Error('libzt CMake layout changed before controller include directories');
    cmake = cmake.replace(anchor, `${anchor}\n${controllerIncludes}`);
  }
  const controllerSources = [
    '         ${ZTO_SRC_DIR}/controller/DB.cpp',
    '         ${ZTO_SRC_DIR}/controller/DBMirrorSet.cpp',
    '         ${ZTO_SRC_DIR}/controller/EmbeddedNetworkController.cpp',
    '         ${ZTO_SRC_DIR}/controller/FileDB.cpp',
    '         ${ZTO_SRC_DIR}/controller/LFDB.cpp',
  ].join('\n');
  if (!cmake.includes(controllerSources)) {
    const priorSources = controllerSources.replace('\n         ${ZTO_SRC_DIR}/controller/LFDB.cpp', '');
    if (cmake.includes(priorSources)) {
      cmake = cmake.replace('         ${ZTO_SRC_DIR}/controller/FileDB.cpp', '         ${ZTO_SRC_DIR}/controller/FileDB.cpp\n         ${ZTO_SRC_DIR}/controller/LFDB.cpp');
    } else {
      const anchor = 'file(GLOB ztcoreSrcGlob ${ZTO_SRC_DIR}/node/*.cpp';
      if (!cmake.includes(anchor)) throw new Error('libzt CMake layout changed before controller sources');
      cmake = cmake.replace(anchor, `file(GLOB ztcoreSrcGlob \${ZTO_SRC_DIR}/node/*.cpp\n${controllerSources}`);
    }
  }
  cmake = cmake.replaceAll(' -DOMIT_JSON_SUPPORT=1', '');
  cmake = cmake.replaceAll('-std=c++11', '-std=c++17');
  // The embedded controller uses C++17. MSVC otherwise parses upstream's
  // non-ASCII comments using the machine ANSI code page, which can consume
  // a following declaration on Chinese Windows due to DBCS line splicing.
  cmake = cmake.replace(' /EHsc -DNOMINMAX', ' /EHsc /utf-8 /std:c++17 -DNOMINMAX');
  if (!cmake.includes('# Link Windows native definitions')) {
    cmake += '\n# Link Windows native definitions\nif(BUILD_WIN)\n    add_compile_definitions(WIN32)\nendif()\n';
  }
  const removeGateway = 'list(REMOVE_ITEM libnatpmpSrcGlob ${ZTO_SRC_DIR}/ext/libnatpmp/wingettimeofday.c)';
  if (!cmake.includes('if(NOT BUILD_WIN)\n' + removeGateway)) {
    cmake = cmake.replace(removeGateway, 'if(NOT BUILD_WIN)\n' + removeGateway + '\nendif()');
  }
  writeFileSync(nativeBuild, cmake);

  // Bootstrap the controller declaration from a pristine SDK as well as an
  // already patched checkout. Earlier local builds hid this missing first step.
  const serviceHeader = join(vend, 'src', 'NodeService.hpp');
  let header = readFileSync(serviceHeader, 'utf8');
  if (!header.includes('    int enableEmbeddedController(bool enabled);')) {
    const anchor = '    int setLowBandwidthMode(bool enabled);';
    if (!header.includes(anchor)) throw Error('Missing controller declaration anchor');
    writeFileSync(serviceHeader, header.replace(anchor, anchor+'\n    int enableEmbeddedController(bool enabled);'));
  }
  modifyNative('src/NodeService.hpp', [
    ['class NodeService;', 'class EmbeddedNetworkController;\nclass NodeService;'],
    ['    int enableEmbeddedController(bool enabled);', '    int enableEmbeddedController(bool enabled);\n    int createControllerNetwork(const char* name, uint64_t* networkId);\n    int authorizeControllerMember(uint64_t networkId, uint64_t memberId);'],
    ['    int authorizeControllerMember(uint64_t networkId, uint64_t memberId);', '    int authorizeControllerMember(uint64_t networkId, uint64_t memberId);\n    int listControllerMembers(uint64_t networkId, char* out, unsigned int* outLen);'],
    ['    std::string _homePath;', '    std::string _homePath;\n    bool _embeddedControllerEnabled = false;\n    EmbeddedNetworkController* _embeddedController = nullptr;'],
  ]);
  modifyNative('src/NodeService.cpp', [
    // ZeroTierOne's ZT_Peer/physical path layouts changed with bonding fields.
    // Convert the public libzt event payload explicitly; memcpy corrupts path_count.
    [`            memcpy(pr, peer, sizeof(zts_peer_info_t));
            for (unsigned int j = 0; j < peer->pathCount; j++) {
                native_ss_to_zts_ss(&(pr->paths[j].address), &(peer->paths[j].address));
            }`,
     `            pr->peer_id = peer->address;
            pr->ver_major = peer->versionMajor;
            pr->ver_minor = peer->versionMinor;
            pr->ver_rev = peer->versionRev;
            pr->latency = peer->latency;
            pr->role = static_cast<zts_peer_role_t>(peer->role);
            pr->unused_0 = peer->isBonded ? 1 : 0;
            pr->path_count = std::min<unsigned int>(peer->pathCount, ZTS_MAX_PEER_NETWORK_PATHS);
            for (unsigned int j = 0; j < pr->path_count; j++) {
                native_ss_to_zts_ss(&(pr->paths[j].address), &(peer->paths[j].address));
                pr->paths[j].last_tx = peer->paths[j].lastSend;
                pr->paths[j].last_rx = peer->paths[j].lastReceive;
                pr->paths[j].trusted_path_id = peer->paths[j].trustedPathId;
                pr->paths[j].expired = peer->paths[j].expired;
                pr->paths[j].preferred = peer->paths[j].preferred;
            }`],
    ['#include "NodeService.hpp"', '#include "NodeService.hpp"\n#include "EmbeddedNetworkController.hpp"'],
    ['            _node = new Node(this, (void*)0, &cb, OSUtils::now());',
     `            _node = new Node(this, (void*)0, &cb, OSUtils::now());
            if (_embeddedControllerEnabled) {
                const std::string controllerPath = _homePath + ZT_PATH_SEPARATOR_S "controller.d";
                _embeddedController = new EmbeddedNetworkController(_node, _homePath.c_str(), controllerPath.c_str(), 0, nullptr);
                _node->setNetconfMaster(_embeddedController);
            }`],
    ['NodeService::~NodeService()\n{\n    _binder.closeAll(_phy);',
     'NodeService::~NodeService()\n{\n    delete _embeddedController;\n    _embeddedController = nullptr;\n    _binder.closeAll(_phy);'],
    ['int NodeService::setHomePath(const char* homePath)',
     `int NodeService::enableEmbeddedController(bool enabled)
{
    Mutex::Lock _lr(_run_m);
    if (_run) return ZTS_ERR_SERVICE;
    _embeddedControllerEnabled = enabled;
    return ZTS_ERR_OK;
}

int NodeService::setHomePath(const char* homePath)`],
    ['int NodeService::enableEmbeddedController(bool enabled)', 'int NodeService::enableEmbeddedController(bool enabled)'],
  ]);
  modifyNative('src/Controls.cpp', [
    ['int zts_init_set_roots(const void* roots_data, unsigned int len)',
     `int zts_init_enable_controller(int enabled)
{
    ACQUIRE_SERVICE_OFFLINE();
    return zts_service->enableEmbeddedController(enabled != 0);
}

int zts_controller_create_network(const char* name, uint64_t* network_id)
{
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    return zts_service->createControllerNetwork(name, network_id);
}

int zts_controller_authorize_member(uint64_t network_id, uint64_t member_id)
{
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    return zts_service->authorizeControllerMember(network_id, member_id);
}

int zts_init_set_roots(const void* roots_data, unsigned int len)`],
    ['int zts_controller_authorize_member(uint64_t network_id, uint64_t member_id)\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);\n    return zts_service->authorizeControllerMember(network_id, member_id);\n}',
     'int zts_controller_authorize_member(uint64_t network_id, uint64_t member_id)\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);\n    return zts_service->authorizeControllerMember(network_id, member_id);\n}\n\nint zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len)\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);\n    return zts_service->listControllerMembers(network_id, out, out_len);\n}'],
  ]);
  modifyNative('ext/ZeroTierOne/controller/EmbeddedNetworkController.hpp', [
    // Keep cpp-httplib's Windows size type private. The public libzt/lwIP ABI
    // uses int ssize_t; changing it would break the native Socket bindings.
    ['#include <cpp-httplib/httplib.h>', '#if defined(_WIN32)\n#define ssize_t link_http_ssize_t\n#include <cpp-httplib/httplib.h>\n#undef ssize_t\n#else\n#include <cpp-httplib/httplib.h>\n#endif'],
    ['\tvoid setSSORedirectURL(const std::string &url);',
     '\tvoid setSSORedirectURL(const std::string &url);\n\tuint64_t createLinkNetwork(const std::string &name);\n\tbool authorizeLinkMember(uint64_t networkId, uint64_t memberId);'],
    ['\tbool authorizeLinkMember(uint64_t networkId, uint64_t memberId);',
     '\tbool authorizeLinkMember(uint64_t networkId, uint64_t memberId);\n\tbool listLinkMembers(uint64_t networkId, std::string &membersJson);'],
  ]);
  modifyNative('ext/ZeroTierOne/controller/EmbeddedNetworkController.cpp', [
    ['void EmbeddedNetworkController::configureHTTPControlPlane(',
     `bool EmbeddedNetworkController::listLinkMembers(uint64_t networkId, std::string &membersJson)
{
    json network, output = json::array();
    std::vector<json> members;
    if (!_db.get(networkId, network, members)) return false;
    for (const auto &member : members) {
        json row = json::object();
        for (const char *key : {"id", "identity", "name", "authorized", "ipAssignments", "lastSeen", "revision"}) {
            if (member.contains(key)) row[key] = member[key];
        }
        output.push_back(row);
    }
    membersJson = output.dump();
    return true;
}

uint64_t EmbeddedNetworkController::createLinkNetwork(const std::string &name)
{
    const uint64_t prefix = (Utils::hexStrToU64(_signingIdAddressString.c_str()) << 24) & 0xffffffffff000000ULL;
    for (unsigned int attempt = 0; attempt < 10000; ++attempt) {
        uint64_t suffix = 0;
        Utils::getSecureRandom(&suffix, sizeof(suffix));
        uint64_t id = prefix | (suffix & 0xffffffULL);
        if ((id & 0xffffffULL) == 0) id |= 1;
        if (_db.hasNetwork(id)) continue;
        json request = {
            {"name", name}, {"private", true}, {"enableBroadcast", true},
            {"v4AssignMode", {{"zt", true}}},
            {"ipAssignmentPools", {{{"ipRangeStart", "10.247.0.1"}, {"ipRangeEnd", "10.247.0.254"}}}},
            {"routes", {{{"target", "10.247.0.0/24"}, {"via", nullptr}}}}
        };
        json created = json::parse(networkUpdateFromPostData(id, request.dump()));
        return OSUtils::jsonIntHex(created["id"], 0ULL);
    }
    return 0;
}

bool EmbeddedNetworkController::authorizeLinkMember(uint64_t networkId, uint64_t memberId)
{
    json network, member;
    if (!_db.get(networkId, network)) return false;
    if (_db.get(networkId, network, memberId, member) && OSUtils::jsonBool(member["authorized"], false)) {
        return true;
    }
    DB::initMember(member);
    char networkHex[17], memberHex[11];
    snprintf(networkHex, sizeof(networkHex), "%016llx", (unsigned long long)networkId);
    snprintf(memberHex, sizeof(memberHex), "%010llx", (unsigned long long)memberId);
    member["id"] = memberHex;
    member["address"] = memberHex;
    member["nwid"] = networkHex;
    if (!OSUtils::jsonBool(member["authorized"], false)) {
        member["authorized"] = true;
        member["lastAuthorizedTime"] = OSUtils::now();
        member["lastAuthorizedCredentialType"] = "link-invite";
        member["lastAuthorizedCredential"] = json();
    }
    DB::cleanMember(member);
    if (_db.save(member, true)) return true;

    // FileDB::save returns false both when persistence fails and when the
    // requested record is already identical. Authorization is idempotent, so
    // verify the stored state before reporting failure to the C API.
    json savedNetwork, savedMember;
    return _db.get(networkId, savedNetwork, memberId, savedMember) &&
        OSUtils::jsonBool(savedMember["authorized"], false);
}

void EmbeddedNetworkController::configureHTTPControlPlane(`],
  ]);
  modifyNative('include/ZeroTierSockets.h', [
    ['ZTS_API int ZTCALL zts_init_set_roots(const void* roots_data, unsigned int len);',
     'ZTS_API int ZTCALL zts_init_enable_controller(int enabled);\nZTS_API int ZTCALL zts_init_set_roots(const void* roots_data, unsigned int len);'],
    ['ZTS_API int ZTCALL zts_init_enable_controller(int enabled);',
     'ZTS_API int ZTCALL zts_controller_create_network(const char* name, uint64_t* network_id);\nZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);\nZTS_API int ZTCALL zts_init_enable_controller(int enabled);'],
    ['ZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);',
     'ZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);\nZTS_API int ZTCALL zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len);'],
    ['ZTS_API int ZTCALL zts_util_sign_root_set(',
     'ZTS_API int ZTCALL zts_util_generate_identity(char* public_id, unsigned int* public_len, char* secret_id, unsigned int* secret_len);\nZTS_API int ZTCALL zts_util_sign_root_set_fresh(char* roots_out, unsigned int* roots_len, char* signing_key, unsigned int* signing_key_len, uint64_t id, uint64_t ts, zts_root_set_t* roots_spec);\n\nZTS_API int ZTCALL zts_util_sign_root_set('],
  ]);
  modify('src/include/ZeroTierSockets.h', [
    ['ZTS_API int ZTCALL zts_init_set_roots(const void* roots_data, unsigned int len);',
     'ZTS_API int ZTCALL zts_init_enable_controller(int enabled);\nZTS_API int ZTCALL zts_init_set_roots(const void* roots_data, unsigned int len);'],
    ['ZTS_API int ZTCALL zts_init_enable_controller(int enabled);',
     'ZTS_API int ZTCALL zts_controller_create_network(const char* name, uint64_t* network_id);\nZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);\nZTS_API int ZTCALL zts_init_enable_controller(int enabled);'],
    ['ZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);',
     'ZTS_API int ZTCALL zts_controller_authorize_member(uint64_t network_id, uint64_t member_id);\nZTS_API int ZTCALL zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len);'],
    ['ZTS_API int ZTCALL zts_util_sign_root_set(',
     'ZTS_API int ZTCALL zts_util_generate_identity(char* public_id, unsigned int* public_len, char* secret_id, unsigned int* secret_len);\nZTS_API int ZTCALL zts_util_sign_root_set_fresh(char* roots_out, unsigned int* roots_len, char* signing_key, unsigned int* signing_key_len, uint64_t id, uint64_t ts, zts_root_set_t* roots_spec);\n\nZTS_API int ZTCALL zts_util_sign_root_set('],
  ]);
  modifyNative('src/Utilities.cpp', [
    ['#include <node/C25519.hpp>', '#include <node/C25519.hpp>\n#include <node/Identity.hpp>'],
    ['#include "ZeroTierSockets.h"', '#include "ZeroTierSockets.h"\n#include <cstring>'],
  ]);
  const utilitiesPath = join(vend, 'src', 'Utilities.cpp');
  let utilities = readFileSync(utilitiesPath, 'utf8');
  if (!utilities.includes('int link_identity_sign(')) {
    const anchor = 'int zts_util_sign_root_set(';
    utilities = utilities.replace(anchor, `int link_identity_sign(const char* secret, const void* message, unsigned int len, void* signature)
{
    if (!secret || !message || !signature || len > 65536) return ZTS_ERR_ARG;
    try { Identity id(secret); if (!id.hasPrivate()) return ZTS_ERR_ARG;
        C25519::Signature signedMessage = id.sign(message, len);
        memcpy(signature, signedMessage.data, ZT_C25519_SIGNATURE_LEN); return ZTS_ERR_OK;
    } catch (...) { return ZTS_ERR_ARG; }
}
int link_identity_verify(const char* publicId, const void* message, unsigned int len, const void* signature)
{
    if (!publicId || !message || !signature || len > 65536) return ZTS_ERR_ARG;
    try { Identity id(publicId); if (id.hasPrivate() || !id.locallyValidate()) return ZTS_ERR_ARG;
        return id.verify(message, len, signature, ZT_C25519_SIGNATURE_LEN) ? ZTS_ERR_OK : ZTS_ERR_ARG;
    } catch (...) { return ZTS_ERR_ARG; }
}

${anchor}`);
  }
  if (!utilities.includes('int zts_util_generate_identity(')) {
    const anchor = 'int zts_util_sign_root_set(';
    if (!utilities.includes(anchor)) throw new Error('libzt Utilities.cpp layout changed before identity helper');
    utilities = utilities.replace(anchor, `int zts_util_generate_identity(char* public_id, unsigned int* public_len, char* secret_id, unsigned int* secret_len)
{
    if (!public_id || !public_len || !secret_id || !secret_len || *public_len < ZT_IDENTITY_STRING_BUFFER_LENGTH || *secret_len < ZT_IDENTITY_STRING_BUFFER_LENGTH) return ZTS_ERR_ARG;
    Identity identity; identity.generate(); identity.toString(false, public_id); identity.toString(true, secret_id);
    *public_len = (unsigned int)std::strlen(public_id); *secret_len = (unsigned int)std::strlen(secret_id);
    return ZTS_ERR_OK;
}

${anchor}`);
  }
  if (!utilities.includes('int zts_util_sign_root_set_fresh(')) {
    const anchor = 'int zts_util_sign_root_set(';
    if (!utilities.includes(anchor)) throw new Error('libzt Utilities.cpp layout changed before Planet signer');
    utilities = utilities.replace(anchor, `int zts_util_sign_root_set_fresh(char* roots_out, unsigned int* roots_len, char* signing_key, unsigned int* signing_key_len, uint64_t id, uint64_t ts, zts_root_set_t* roots_spec)
{
    if (!roots_out || !roots_len || !signing_key || !signing_key_len || !roots_spec || *signing_key_len < (ZT_C25519_PUBLIC_KEY_LEN + ZT_C25519_PRIVATE_KEY_LEN)) return ZTS_ERR_ARG;
    C25519::Pair key(C25519::generate()); std::vector<World::Root> roots;
    for (int i = 0; i < ZTS_MAX_NUM_ROOTS && roots_spec->public_id_str[i]; ++i) {
        roots.emplace_back(); roots.back().identity = Identity(roots_spec->public_id_str[i]);
        for (int j = 0; j < ZTS_MAX_ENDPOINTS_PER_ROOT && roots_spec->endpoint_ip_str[i][j]; ++j) roots.back().stableEndpoints.emplace_back(roots_spec->endpoint_ip_str[i][j]);
    }
    if (roots.empty()) return ZTS_ERR_ARG;
    World world = World::make(World::TYPE_PLANET, id, ts, key.pub, roots, key);
    Buffer<ZT_WORLD_MAX_SERIALIZED_LENGTH> out; world.serialize(out, false);
    if (out.size() > *roots_len) return ZTS_ERR_ARG;
    std::memcpy(roots_out, out.data(), out.size()); *roots_len = (unsigned int)out.size();
    std::memcpy(signing_key, key.pub.data, ZT_C25519_PUBLIC_KEY_LEN); std::memcpy(signing_key + ZT_C25519_PUBLIC_KEY_LEN, key.priv.data, ZT_C25519_PRIVATE_KEY_LEN); *signing_key_len = ZT_C25519_PUBLIC_KEY_LEN + ZT_C25519_PRIVATE_KEY_LEN;
    return ZTS_ERR_OK;
}

${anchor}`);
  }
  // Repair the previously generated fresh signer as well as fresh checkouts.
  // C25519 stores a DH and an Ed25519 key: each public/private half is 64 bytes.
  utilities = utilities
    .replace('!roots_spec || *signing_key_len < 64)', '!roots_spec || *signing_key_len < (ZT_C25519_PUBLIC_KEY_LEN + ZT_C25519_PRIVATE_KEY_LEN))')
    .replace('std::memcpy(signing_key, key.pub.data, 32); std::memcpy(signing_key + 32, key.priv.data, 32); *signing_key_len = 64;',
      'std::memcpy(signing_key, key.pub.data, ZT_C25519_PUBLIC_KEY_LEN); std::memcpy(signing_key + ZT_C25519_PUBLIC_KEY_LEN, key.priv.data, ZT_C25519_PRIVATE_KEY_LEN); *signing_key_len = ZT_C25519_PUBLIC_KEY_LEN + ZT_C25519_PRIVATE_KEY_LEN;');
  writeFileSync(utilitiesPath, utilities);
  const controllerExport=join(vend,'ext/ZeroTierOne/controller/EmbeddedNetworkController.cpp');
  const exportBody=readFileSync(controllerExport,'utf8');
  writeFileSync(controllerExport,exportBody.replace('{"id", "name", "authorized", "ipAssignments", "lastSeen", "revision"}', '{"id", "identity", "name", "authorized", "ipAssignments", "lastSeen", "revision"}'));
  // Member removal goes through the running Controller DB, never edits its files.
  modifyNative('src/NodeService.hpp', [['    int listControllerMembers(uint64_t networkId, char* out, unsigned int* outLen);', '    int listControllerMembers(uint64_t networkId, char* out, unsigned int* outLen);\n    int deauthorizeControllerMember(uint64_t networkId, uint64_t memberId);']]);
  modifyNative('src/NodeService.cpp', [['int NodeService::listControllerMembers(', `int NodeService::deauthorizeControllerMember(uint64_t networkId, uint64_t memberId)
{
    if (!_embeddedController) return ZTS_ERR_SERVICE;
    return _embeddedController->deauthorizeLinkMember(networkId, memberId) ? ZTS_ERR_OK : ZTS_ERR_GENERAL;
}

int NodeService::listControllerMembers(`]]);
  modifyNative('ext/ZeroTierOne/controller/EmbeddedNetworkController.hpp', [['\tbool listLinkMembers(uint64_t networkId, std::string &membersJson);', '\tbool listLinkMembers(uint64_t networkId, std::string &membersJson);\n\tbool deauthorizeLinkMember(uint64_t networkId, uint64_t memberId);']]);
  modifyNative('ext/ZeroTierOne/controller/EmbeddedNetworkController.cpp', [['bool EmbeddedNetworkController::listLinkMembers(', `bool EmbeddedNetworkController::deauthorizeLinkMember(uint64_t networkId, uint64_t memberId)
{
    json network, member;
    if (!_db.get(networkId, network, memberId, member)) return false;
    if (!OSUtils::jsonBool(member["authorized"], false)) return true;
    member["authorized"] = false;
    member["lastDeauthorizedTime"] = OSUtils::now();
    DB::cleanMember(member);
    if (_db.save(member, true)) return true;
    json savedNetwork, savedMember;
    return _db.get(networkId, savedNetwork, memberId, savedMember) && !OSUtils::jsonBool(savedMember["authorized"], true);
}

bool EmbeddedNetworkController::listLinkMembers(`]]);
  modifyNative('src/Controls.cpp', [['int zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len)\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);\n    return zts_service->listControllerMembers(network_id, out, out_len);\n}', `int zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len)
{
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    return zts_service->listControllerMembers(network_id, out, out_len);
}

int zts_controller_deauthorize_member(uint64_t network_id, uint64_t member_id)
{
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    return zts_service->deauthorizeControllerMember(network_id, member_id);
}`]]);
  for (const header of ['include/ZeroTierSockets.h', 'pkg/crate/libzt/src/include/ZeroTierSockets.h']) {
    modifyNative(header, [['ZTS_API int ZTCALL zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len);', 'ZTS_API int ZTCALL zts_controller_list_members(uint64_t network_id, char* out, unsigned int* out_len);\nZTS_API int ZTCALL zts_controller_deauthorize_member(uint64_t network_id, uint64_t member_id);']]);
  }
  patchLibztPacketAdapter(vend);
  console.log('libzt compatibility patches checked');
}
