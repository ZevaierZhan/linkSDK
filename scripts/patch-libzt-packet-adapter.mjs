import fs from 'node:fs';
import path from 'node:path';
export function patchLibztPacketAdapter(vendor) {
  const owned=name=>fs.readFileSync(new URL('./libzt-packet-adapter/'+name,import.meta.url),'utf8').replaceAll('\r\n','\n');
  const patch=(file,before,after)=>{
    const target=path.join(vendor,file),source=fs.readFileSync(target,'utf8').replaceAll('\r\n','\n');
    if(source.includes(after))return;
    if(source.split(before).length!==2)throw Error(`Pinned libzt packet adapter anchor changed: ${file}`);
    fs.writeFileSync(target,source.replace(before,after));
  };
  for(const name of ['LinkPacketAdapter.hpp','LinkPacketAdapter.cpp'])fs.writeFileSync(path.join(vendor,'src',name),owned(name));
  for(const header of ['include/ZeroTierSockets.h','pkg/crate/libzt/src/include/ZeroTierSockets.h']){
    const anchor='ZTS_API int ZTCALL zts_init_from_storage(const char* path);';
    patch(header,anchor,anchor+'\n\n'+owned('api.h'));
  }
  for(const file of ['src/Controls.cpp','src/NodeService.cpp','src/VirtualTap.cpp']){
    const anchor=file==='src/NodeService.cpp'?'#include "VirtualTap.hpp"':'#include "Events.hpp"';
    patch(file,anchor,anchor+'\n#include "LinkPacketAdapter.hpp"');
  }
  patch('src/Controls.cpp','#include "LinkPacketAdapter.hpp"','#include "LinkPacketAdapter.hpp"\n#include <mutex>\n#include <condition_variable>\n#include <thread>\n#include <chrono>');
  patch('src/Controls.cpp','Mutex service_m;',`Mutex service_m;
// Stored only for external mode. Completion is followed by a real OS join;
// NODE_DOWN and a null service pointer alone are not lifetime evidence.
static std::timed_mutex linkPacketJoinMutex;
static std::condition_variable_any linkPacketJoinDone;
static bool linkPacketJoinRunning = false;
static int linkPacketJoinResult = ZTS_ERR_SERVICE;
static uint64_t linkPacketJoinEpoch = 0;
static bool linkPacketServiceJoinable = false, linkPacketCallbackJoinable = false;
#if defined(__WINDOWS__)
static HANDLE linkPacketServiceThread, linkPacketCallbackThread;
#else
static pthread_t linkPacketServiceThread, linkPacketCallbackThread;
#endif
static void linkPacketStartFailed()
{
    zts_events->clrState(ZTS_STATE_CALLBACKS_RUNNING);
    delete zts_service;
    zts_service = (NodeService*)0;
    LinkPacketAdapter::startFailed();
}`);
  patch('src/Controls.cpp','int zts_init_from_storage(const char* path)',`int zts_init_set_packet_adapter(uint32_t abi, zts_packet_frame_callback callback, void* context)
{
    if (LinkPacketAdapter::inCallback()) return ZTS_ERR_ARG;
    ACQUIRE_SERVICE_OFFLINE();
    return LinkPacketAdapter::configure(abi, callback, context);
}
int zts_packet_adapter_get_generation(uint64_t* generation)
{
    if (generation) *generation = 0;
    if (!generation || LinkPacketAdapter::inCallback() || !LinkPacketAdapter::enabled()) return ZTS_ERR_ARG;
    *generation = LinkPacketAdapter::generation();
    return ZTS_ERR_OK;
}
int zts_packet_adapter_set_ready(uint64_t generation, uint64_t network, int ready)
{
    if (LinkPacketAdapter::inCallback() || (ready != 0 && ready != 1)) return ZTS_ERR_ARG;
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    int result = zts_service->linkSetPacketReady(generation, network, ready != 0);
    if (result == ZTS_ERR_OK) zts_events->setState(0);
    return result;
}
int zts_packet_adapter_inject(uint64_t generation, uint64_t network, uint64_t source,
    uint64_t destination, uint32_t type, uint32_t vlan, const void* payload, uint32_t length)
{
    if (LinkPacketAdapter::inCallback()) return ZTS_ERR_ARG;
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    if (!LinkPacketAdapter::enterTx(generation, network)) return ZTS_ERR_SERVICE;
    struct Leave { ~Leave() { LinkPacketAdapter::leave(); } } leave;
    return zts_service->linkInjectPacket(network, source, destination, type, vlan, payload, length);
}
int zts_packet_adapter_stop(uint64_t generation, uint32_t timeout_ms)
{
    return LinkPacketAdapter::stop(generation, timeout_ms);
}
int zts_packet_adapter_join(uint64_t generation, uint32_t timeout_ms)
{
    if (LinkPacketAdapter::inCallback() || !LinkPacketAdapter::enabled() || timeout_ms > 60000) return ZTS_ERR_ARG;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
    std::unique_lock<std::timed_mutex> joinLock(linkPacketJoinMutex, std::defer_lock);
    if (!joinLock.try_lock_until(deadline)) return ZTS_ERR_SERVICE;
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - std::chrono::steady_clock::now()).count();
    int result = LinkPacketAdapter::waitEnded(generation, remaining > 0 ? (uint32_t)remaining : 0);
    if (result != ZTS_ERR_OK) return result;
    if (!linkPacketJoinRunning && linkPacketJoinEpoch != generation) {
        linkPacketJoinRunning = true;
        linkPacketJoinEpoch = generation;
        try {
            // pthread_join has no timed form on Apple platforms. Keep its real
            // ownership work alive if the caller's bounded wait expires.
            std::thread([] {
                int joinedResult = ZTS_ERR_OK;
                if (linkPacketCallbackJoinable) {
#if defined(__WINDOWS__)
                    if (WaitForSingleObject(linkPacketCallbackThread, INFINITE) != WAIT_OBJECT_0) joinedResult = ZTS_ERR_SERVICE;
                    else { CloseHandle(linkPacketCallbackThread); linkPacketCallbackJoinable = false; }
#else
                    if (pthread_join(linkPacketCallbackThread, NULL) != 0) joinedResult = ZTS_ERR_SERVICE;
                    else linkPacketCallbackJoinable = false;
#endif
                }
                if (linkPacketServiceJoinable) {
#if defined(__WINDOWS__)
                    if (WaitForSingleObject(linkPacketServiceThread, INFINITE) != WAIT_OBJECT_0) joinedResult = ZTS_ERR_SERVICE;
                    else { CloseHandle(linkPacketServiceThread); linkPacketServiceJoinable = false; }
#else
                    if (pthread_join(linkPacketServiceThread, NULL) != 0) joinedResult = ZTS_ERR_SERVICE;
                    else linkPacketServiceJoinable = false;
#endif
                }
                { Mutex::Lock lock(service_m); if (zts_service) joinedResult = ZTS_ERR_SERVICE; }
                std::lock_guard<std::timed_mutex> lock(linkPacketJoinMutex);
                if (joinedResult == ZTS_ERR_OK) joinedResult = LinkPacketAdapter::joined();
                linkPacketJoinResult = joinedResult;
                linkPacketJoinRunning = false;
                linkPacketJoinDone.notify_all();
            }).detach();
        } catch (...) {
            linkPacketJoinRunning = false;
            linkPacketJoinEpoch = 0;
            return ZTS_ERR_SERVICE;
        }
    }
    if (!linkPacketJoinDone.wait_until(joinLock, deadline, [] { return !linkPacketJoinRunning; })) return ZTS_ERR_SERVICE;
    return linkPacketJoinResult;
}

int zts_init_from_storage(const char* path)`);
  patch('src/Controls.cpp','    // Start TCP/IP stack\n    zts_lwip_driver_init();',`    // LINK_EXCLUSIVE_PACKET_ADAPTER: never give the same IP to two stacks.
    if (LinkPacketAdapter::begin() != ZTS_ERR_OK) return ZTS_ERR_SERVICE;
    if (!LinkPacketAdapter::enabled()) zts_lwip_driver_init();`);
  patch('src/Controls.cpp','int zts_node_stop()\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);',`int zts_node_stop()
{
    if (LinkPacketAdapter::inCallback()) return ZTS_ERR_ARG;
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    LinkPacketAdapter::quiesce();`);
  patch('src/Controls.cpp','    zts_lwip_driver_shutdown();\n    delete zts_events;',`    if (!LinkPacketAdapter::enabled()) zts_lwip_driver_shutdown();
    LinkPacketAdapter::quiesce();
    delete zts_events;`);
  patch('src/Controls.cpp','int zts_node_free()\n{\n    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);',`int zts_node_free()
{
    // This legacy API deletes Events without joining its callback thread.
    // External owners must request stop, drain and join instead.
    if (LinkPacketAdapter::enabled()) return ZTS_ERR_ARG;
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);`);
  patch('src/Controls.cpp','    zts_events->run();','    zts_events->run();\n    if (LinkPacketAdapter::enabled()) LinkPacketAdapter::callbackEnded();');
  patch('src/Controls.cpp','        if (zts_events) {\n            zts_events->disable();','        if (zts_events) {\n            if (LinkPacketAdapter::enabled()) zts_events->clrState(ZTS_STATE_CALLBACKS_RUNNING);\n            zts_events->disable();');
  patch('src/Controls.cpp','#ifndef __WINDOWS__\n    pthread_exit(0);\n#endif\n    return NULL;',`    if (LinkPacketAdapter::enabled()) LinkPacketAdapter::serviceEnded();
#ifndef __WINDOWS__
    pthread_exit(0);
#endif
    return NULL;`);
  patch('src/Controls.cpp','    if (zts_events->hasCallback()) {\n        zts_events->setState', '    if (zts_events->hasCallback()) {\n        // LINK_EXTERNAL_CALLBACK_LIFETIME\n        if (LinkPacketAdapter::enabled()) LinkPacketAdapter::callbackStarted();\n        zts_events->setState');
  patch('src/Controls.cpp','        if (callbackThread) CloseHandle(callbackThread);',`        if (callbackThread) {
            if (LinkPacketAdapter::enabled()) { linkPacketCallbackThread = callbackThread; linkPacketCallbackJoinable = true; }
            else CloseHandle(callbackThread);
        }`);
  patch('src/Controls.cpp','        res = pthread_create(&cbThread, NULL, cbRun, NULL);','        res = pthread_create(&cbThread, NULL, cbRun, NULL);\n        if (res == 0 && LinkPacketAdapter::enabled()) { linkPacketCallbackThread = cbThread; linkPacketCallbackJoinable = true; }');
  patch('src/Controls.cpp','            zts_events->clrCallback();\n            return ZTS_ERR_SERVICE;',`            zts_events->clrCallback();
            if (LinkPacketAdapter::enabled()) { LinkPacketAdapter::callbackEnded(); linkPacketStartFailed(); }
            return ZTS_ERR_SERVICE;`);
  patch('src/Controls.cpp','    // TODO: Check success\n#else',`    if (LinkPacketAdapter::enabled()) {
        res = serviceThread ? ZTS_ERR_OK : ZTS_ERR_SERVICE;
        if (serviceThread) { linkPacketServiceThread = serviceThread; linkPacketServiceJoinable = true; }
    }
#else`);
  patch('src/Controls.cpp','    if ((res = pthread_create(&service_thread, NULL, _runNodeService, (void*)NULL)) != 0) {}','    if ((res = pthread_create(&service_thread, NULL, _runNodeService, (void*)NULL)) != 0) {}\n    if (res == 0 && LinkPacketAdapter::enabled()) { linkPacketServiceThread = service_thread; linkPacketServiceJoinable = true; }');
  patch('src/Controls.cpp','    if (res != ZTS_ERR_OK) {\n        zts_events->clrState(ZTS_STATE_NODE_RUNNING);','    if (res != ZTS_ERR_OK) {\n        if (LinkPacketAdapter::enabled()) { linkPacketStartFailed(); return ZTS_ERR_SERVICE; }\n        zts_events->clrState(ZTS_STATE_NODE_RUNNING);');
  patch('src/Controls.cpp','    zts_events->setState(ZTS_STATE_NODE_RUNNING);\n    return ZTS_ERR_OK;','    zts_events->setState(ZTS_STATE_NODE_RUNNING);\n    if (LinkPacketAdapter::enabled()) LinkPacketAdapter::launchComplete();\n    return ZTS_ERR_OK;');
  patch('src/VirtualTap.cpp','    // Start virtual tap thread and stack I/O loops\n    _thread = Thread::start(this);',`    // LINK_EXCLUSIVE_PACKET_ADAPTER: an OS-owned interface has no lwIP tap thread.
    if (!LinkPacketAdapter::enabled()) _thread = Thread::start(this);`);
  patch('src/VirtualTap.cpp','    zts_lwip_remove_netif(netif4);\n    netif4 = NULL;\n    zts_lwip_remove_netif(netif6);\n    netif6 = NULL;\n    Thread::join(_thread);',`    if (!LinkPacketAdapter::enabled()) {
        zts_lwip_remove_netif(netif4);
        zts_lwip_remove_netif(netif6);
        Thread::join(_thread);
    }
    netif4 = NULL;
    netif6 = NULL;`);
  patch('src/VirtualTap.cpp','        zts_lwip_init_interface((void*)this, ip);','        if (!LinkPacketAdapter::enabled()) zts_lwip_init_interface((void*)this, ip);');
  patch('src/VirtualTap.cpp','        zts_lwip_remove_address_from_netif((void*)this, ip);','        if (!LinkPacketAdapter::enabled()) zts_lwip_remove_address_from_netif((void*)this, ip);');
  patch('src/NodeService.cpp','    n->tap->put(MAC(sourceMac), MAC(destMac), etherType, data, len);',`    if (LinkPacketAdapter::enabled()) {
        LinkPacketAdapter::deliver(net_id, sourceMac, destMac, etherType, vlanId, data, len);
    } else n->tap->put(MAC(sourceMac), MAC(destMac), etherType, data, len);`);
  patch('src/NodeService.cpp','    delete _node;\n    _node = (Node*)0;',`    // No new RX/TX; drain foreign callback/injection owners before core deletion.
    LinkPacketAdapter::drainCore();
    delete _node;
    _node = (Node*)0;`);
  patch('src/NodeService.cpp','            memcpy(&(n.config), nwc, sizeof(ZT_VirtualNetworkConfig));','            if (LinkPacketAdapter::enabled()) LinkPacketAdapter::invalidateConfig(net_id);\n            memcpy(&(n.config), nwc, sizeof(ZT_VirtualNetworkConfig));');
  patch('src/NodeService.cpp','        case ZT_VIRTUAL_NETWORK_CONFIG_OPERATION_DESTROY:\n            sendEventToUser',`        case ZT_VIRTUAL_NETWORK_CONFIG_OPERATION_DESTROY:
            LinkPacketAdapter::invalidateConfig(net_id);
            sendEventToUser`);
  patch('src/NodeService.cpp','    if (! _node->online() || ! zts_lwip_is_up()) {','    if (! _node->online() || (!LinkPacketAdapter::enabled() && !zts_lwip_is_up())) {');
  patch('src/NodeService.cpp','if (tap->hasIpv4Addr() && zts_lwip_is_netif_up(tap->netif4)) {','if (tap->hasIpv4Addr() && (LinkPacketAdapter::enabled() ? LinkPacketAdapter::ready(n->first) : zts_lwip_is_netif_up(tap->netif4))) {');
  patch('src/NodeService.cpp','if (tap->hasIpv6Addr() && zts_lwip_is_netif_up(tap->netif6)) {','if (tap->hasIpv6Addr() && !LinkPacketAdapter::enabled() && zts_lwip_is_netif_up(tap->netif6)) {');
  patch('src/NodeService.cpp','    return netState.config.assignedAddressCount > 0;','    return netState.config.assignedAddressCount > 0 && (!LinkPacketAdapter::enabled() || LinkPacketAdapter::ready(net_id));');
  // Packet-adapter readiness is not lwIP socket readiness. Preserve Events'
  // NODE+STACK-derived NET flag, and make the socket predicate require STACK
  // explicitly so a future packet-ready state cannot call uninitialized lwIP.
  patch('src/Events.hpp','    last_state_check = service_state & ZTS_STATE_NET_SERVICE_RUNNING;',
    '    last_state_check = (service_state & ZTS_STATE_NET_SERVICE_RUNNING) && (service_state & ZTS_STATE_STACK_RUNNING) ? ZTS_STATE_NET_SERVICE_RUNNING : 0;');
  patch('src/NodeService.hpp','    void tapFrameHandler(',`    int linkSetPacketReady(uint64_t generation, uint64_t network, bool ready);
    int linkInjectPacket(uint64_t network, uint64_t source, uint64_t destination,
        uint32_t type, uint32_t vlan, const void* payload, uint32_t length);
    void tapFrameHandler(`);
  patch('src/NodeService.cpp','void NodeService::tapFrameHandler(',`int NodeService::linkSetPacketReady(uint64_t generation, uint64_t network, bool ready)
{
    Mutex::Lock lock(_nets_m);
    std::map<uint64_t, NetworkState>::iterator n = _nets.find(network);
    if (n == _nets.end() || !n->second.tap || (ready && (n->second.config.status != ZT_NETWORK_STATUS_OK || !n->second.config.assignedAddressCount))) return ZTS_ERR_SERVICE;
    int result = LinkPacketAdapter::setReady(generation, network, ready);
    if (result == ZTS_ERR_OK) n->second.tap->_networkStatus = -1;
    return result;
}
int NodeService::linkInjectPacket(uint64_t network, uint64_t source, uint64_t destination,
    uint32_t type, uint32_t vlan, const void* payload, uint32_t length)
{
    if (!payload || !length || vlan || (type != 0x0800 && type != 0x0806) || !destination || (destination >> 48) || (source >> 48)) return ZTS_ERR_ARG;
    {
        Mutex::Lock lock(_nets_m);
        std::map<uint64_t, NetworkState>::iterator n = _nets.find(network);
        if (n == _nets.end() || !n->second.tap || n->second.config.status != ZT_NETWORK_STATUS_OK) return ZTS_ERR_SERVICE;
        if (source != n->second.config.mac || length > n->second.config.mtu) return ZTS_ERR_ARG;
    }
    if (!_node) return ZTS_ERR_SERVICE;
    return (int)_node->processVirtualNetworkFrame((void*)0, OSUtils::now(), network,
        source, destination, type, vlan, payload, length, &_nextBackgroundTaskDeadline);
}

void NodeService::tapFrameHandler(`);
}
