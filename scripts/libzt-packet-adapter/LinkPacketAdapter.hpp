#pragma once
#include <stdint.h>
#include "ZeroTierSockets.h"

// Internal implementation seam. No platform framework or application Runtime.
namespace LinkPacketAdapter {
bool enabled();
bool inCallback();
int configure(uint32_t abi, zts_packet_frame_callback callback, void *context);
int begin();
uint64_t generation();
bool ready(uint64_t network);
bool anyReady();
int setReady(uint64_t generation, uint64_t network, bool ready);
void forget(uint64_t network);
void invalidateConfig(uint64_t network);
bool enterTx(uint64_t generation, uint64_t network);
void leave();
void deliver(uint64_t network, uint64_t source, uint64_t destination,
             uint32_t type, uint32_t vlan, const void *data, uint32_t length);
void quiesce();
int stop(uint64_t generation, uint32_t timeoutMs);
void drainCore();
void launchComplete();
void callbackStarted();
void callbackEnded();
void serviceEnded();
void startFailed();
int waitEnded(uint64_t generation,uint32_t timeoutMs);
int joined();
}
