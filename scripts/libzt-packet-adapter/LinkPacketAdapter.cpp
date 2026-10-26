#include "LinkPacketAdapter.hpp"
#include <mutex>
#include <condition_variable>
#include <chrono>
#include <set>
#include <limits>

namespace LinkPacketAdapter {
namespace {
struct State {
  std::mutex mutex;
  std::condition_variable drained;
  zts_packet_frame_callback callback = nullptr;
  void *context = nullptr;
  uint64_t epoch = 0, stoppingEpoch = 0;
  bool legacyStarted = false;
  bool accepting = false, coreRunning = false, serviceDone = true, callbackDone = true, launchDone = true;
  size_t inFlight = 0;
  std::set<uint64_t> networks;
};
State state;
thread_local bool delivering = false;
void quiesceLocked() {
  if (state.accepting) {
    state.stoppingEpoch = state.epoch;
    state.accepting = false;
    state.networks.clear();
    if (state.epoch != std::numeric_limits<uint64_t>::max()) ++state.epoch;
  }
}
}
bool inCallback() { return delivering; }
bool enabled() { std::lock_guard<std::mutex> lock(state.mutex); return state.callback != nullptr; }
int configure(uint32_t abi, zts_packet_frame_callback callback, void *context) {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (abi != 1 || (!callback && context) || delivering) return ZTS_ERR_ARG;
  if ((state.legacyStarted && callback) || state.coreRunning || state.accepting || state.inFlight || state.epoch == std::numeric_limits<uint64_t>::max()) return ZTS_ERR_SERVICE;
  state.callback = callback; state.context = context; state.networks.clear();
  state.stoppingEpoch = 0; ++state.epoch;
  return ZTS_ERR_OK;
}
int begin() {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (!state.callback) { state.legacyStarted = true; return ZTS_ERR_OK; }
  if (state.coreRunning || state.inFlight || state.accepting || state.epoch == std::numeric_limits<uint64_t>::max()) return ZTS_ERR_SERVICE;
  state.coreRunning = true; state.serviceDone = false; state.callbackDone = true; state.launchDone = false;
  ++state.epoch; state.stoppingEpoch = 0; state.networks.clear(); state.accepting = true;
  return ZTS_ERR_OK;
}
uint64_t generation() { std::lock_guard<std::mutex> lock(state.mutex); return state.epoch; }
bool ready(uint64_t network) {
  std::lock_guard<std::mutex> lock(state.mutex);
  return state.accepting && state.networks.count(network) != 0;
}
bool anyReady() {
  std::lock_guard<std::mutex> lock(state.mutex);
  return state.accepting && !state.networks.empty();
}
int setReady(uint64_t epoch, uint64_t network, bool value) {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (delivering || !network) return ZTS_ERR_ARG;
  if (!state.accepting || state.epoch != epoch) return ZTS_ERR_SERVICE;
  if (value) {
    if (!state.networks.count(network) && state.networks.size() >= 16) return ZTS_ERR_SERVICE;
    state.networks.insert(network);
  } else state.networks.erase(network);
  return ZTS_ERR_OK;
}
void forget(uint64_t network) { std::lock_guard<std::mutex> lock(state.mutex); state.networks.erase(network); }
void invalidateConfig(uint64_t network) {
  std::lock_guard<std::mutex> lock(state.mutex);
  state.networks.erase(network);
  if (state.accepting) {
    state.networks.clear();
    if (state.epoch == std::numeric_limits<uint64_t>::max()) quiesceLocked();
    else ++state.epoch;
  }
}
bool enterTx(uint64_t epoch, uint64_t network) {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (delivering || !state.accepting || state.epoch != epoch || !state.networks.count(network)) return false;
  ++state.inFlight; return true;
}
void leave() {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (--state.inFlight == 0) state.drained.notify_all();
}
void deliver(uint64_t network, uint64_t source, uint64_t destination,
             uint32_t type, uint32_t vlan, const void *data, uint32_t length) {
  zts_packet_frame_callback callback;
  void *context; uint64_t epoch;
  {
    std::lock_guard<std::mutex> lock(state.mutex);
    if (!state.accepting || !state.callback || !state.networks.count(network) || delivering ||
        !data || !length || length > 65535 || vlan || (type != 0x0800 && type != 0x0806)) return;
    callback = state.callback; context = state.context; epoch = state.epoch; ++state.inFlight;
  }
  delivering = true;
  // Foreign callbacks MUST NOT throw or call the libzt API. Always leave our
  // lifetime accounting consistent even if a C++ embedding violates that rule.
  try { callback(context, epoch, network, source, destination, type, vlan, data, length); }
  catch (...) { delivering = false; leave(); return; }
  delivering = false; leave();
}
void quiesce() { std::lock_guard<std::mutex> lock(state.mutex); quiesceLocked(); }
int stop(uint64_t epoch, uint32_t timeoutMs) {
  if (delivering || timeoutMs > 60000) return ZTS_ERR_ARG;
  std::unique_lock<std::mutex> lock(state.mutex);
  if (!state.callback || (epoch != state.epoch && epoch != state.stoppingEpoch)) return ZTS_ERR_SERVICE;
  quiesceLocked();
  return state.drained.wait_for(lock, std::chrono::milliseconds(timeoutMs), [] { return state.inFlight == 0; }) ? ZTS_ERR_OK : ZTS_ERR_SERVICE;
}
void launchComplete() { std::lock_guard<std::mutex> lock(state.mutex); state.launchDone = true; state.drained.notify_all(); }
void callbackStarted() { std::lock_guard<std::mutex> lock(state.mutex); state.callbackDone = false; }
void callbackEnded() { std::lock_guard<std::mutex> lock(state.mutex); state.callbackDone = true; state.drained.notify_all(); }
void serviceEnded() { std::lock_guard<std::mutex> lock(state.mutex); state.serviceDone = true; state.drained.notify_all(); }
void startFailed() { std::lock_guard<std::mutex> lock(state.mutex); quiesceLocked(); state.serviceDone = true; state.launchDone = true; state.drained.notify_all(); }
int waitEnded(uint64_t epoch,uint32_t timeoutMs) {
  if (delivering || timeoutMs > 60000) return ZTS_ERR_ARG;
  std::unique_lock<std::mutex> lock(state.mutex);
  if (!state.callback || (epoch != state.epoch && epoch != state.stoppingEpoch)) return ZTS_ERR_SERVICE;
  return state.drained.wait_for(lock,std::chrono::milliseconds(timeoutMs),[]{return state.launchDone && state.serviceDone && state.callbackDone && state.inFlight == 0;}) ? ZTS_ERR_OK : ZTS_ERR_SERVICE;
}
int joined() {
  std::lock_guard<std::mutex> lock(state.mutex);
  if (!state.launchDone || !state.serviceDone || !state.callbackDone || state.inFlight || state.accepting) return ZTS_ERR_SERVICE;
  state.coreRunning = false;
  return ZTS_ERR_OK;
}
void drainCore() {
  std::unique_lock<std::mutex> lock(state.mutex);
  quiesceLocked();
  // Core teardown cannot delete the node while an injection or callback uses
  // it. A wedged embedding remains honestly stopped-pending, never freed early.
  state.drained.wait(lock, [] { return state.inFlight == 0; });
}
}
