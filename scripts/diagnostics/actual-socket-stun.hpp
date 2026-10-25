#pragma once

// Opt-in, bounded diagnostic for the actual libzt UDP descriptor. It never
// creates/binds sockets or feeds STUN observations into ZeroTier path state.
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <mutex>
#include <string>
#include "Utils.hpp"

namespace ZeroTier { namespace LinkSocketStun {
inline uint64_t nowMs() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::system_clock::now().time_since_epoch()).count();
}
inline bool enabled() {
    static const bool value = [] {
        const char* path = std::getenv("LINK_DIAG_STUN_FILE");
        const char* bind = std::getenv("LINK_DIAG_STUN_BIND");
        return path && *path && bind && *bind;
    }();
    return value;
}
inline uint32_t ipv4(const char* text) {
    in_addr address = {};
    return text && ::inet_pton(AF_INET, text, &address) == 1 ? address.s_addr : 0;
}
inline std::string hex(const unsigned char* p, unsigned n) {
    static const char* chars = "0123456789abcdef";
    std::string out;
    for (unsigned i = 0; i < n; ++i) { out += chars[p[i] >> 4]; out += chars[p[i] & 15]; }
    return out;
}
inline std::string endpoint(const sockaddr_in& a) {
    const unsigned char* p = reinterpret_cast<const unsigned char*>(&a.sin_addr.s_addr);
    char out[64];
    std::snprintf(out, sizeof(out), "%u.%u.%u.%u:%u", p[0], p[1], p[2], p[3], ntohs(a.sin_port));
    return out;
}
struct Pending {
    uint64_t fd, sent;
    sockaddr_in local, remote;
    bool complete;
};
struct Round { unsigned count = 0; uint64_t last = 0; };
struct State {
    std::mutex mutex;
    std::map<std::string, Pending> pending;
    std::map<std::string, Round> rounds;
};
inline State& state() { static State value; return value; }
inline void log(const char* event, uint64_t fd, const sockaddr_in& local,
                const sockaddr_in& remote, const std::string& tx,
                const std::string& mapped, int result, uint64_t elapsed = 0) {
    const char* path = std::getenv("LINK_DIAG_STUN_FILE");
    FILE* f = path ? std::fopen(path, "ab") : nullptr;
    if (!f) return;
    std::fprintf(f, "{\"time_ms\":%llu,\"event\":\"%s\",\"fd\":%llu,\"local\":\"%s\",\"remote\":\"%s\",\"transaction\":\"%s\",\"mapped\":\"%s\",\"result\":%d,\"elapsed_ms\":%llu}\n",
        (unsigned long long)nowMs(), event, (unsigned long long)fd,
        endpoint(local).c_str(), endpoint(remote).c_str(), tx.c_str(), mapped.c_str(), result,
        (unsigned long long)elapsed);
    std::fclose(f);
}
inline void maybeSend(ZT_PHY_SOCKFD_TYPE fd, const sockaddr_storage* destination) {
    if (!enabled() || destination->ss_family != AF_INET) return;
    const sockaddr_in& root = *reinterpret_cast<const sockaddr_in*>(destination);
    if (ntohs(root.sin_port) != 19993 || root.sin_addr.s_addr != ipv4("112.74.105.184")) return;
    sockaddr_in local = {};
#if defined(_WIN32) || defined(_WIN64)
    int size = sizeof(local);
#else
    socklen_t size = sizeof(local);
#endif
    if (::getsockname(fd, reinterpret_cast<sockaddr*>(&local), &size) || local.sin_family != AF_INET) return;
    if (local.sin_addr.s_addr != ipv4(std::getenv("LINK_DIAG_STUN_BIND"))) return;
    State& s = state();
    std::lock_guard<std::mutex> lock(s.mutex);
    const std::string key = std::to_string((uint64_t)fd) + "/" + endpoint(local);
    if (!s.rounds.count(key) && s.rounds.size() >= 8) return;
    Round& round = s.rounds[key];
    const uint64_t now = nowMs();
    if (round.count >= 2 || (round.count && (now < round.last || now - round.last < 15000))) return;
    ++round.count; round.last = now;
    log("root_send_same_descriptor", (uint64_t)fd, local, root, "", "", round.count);
    // A short, recognizable marker lets Root correlate this exact descriptor
    // with its observed public tuple. It is below ZeroTier's packet minimum.
    unsigned char marker[12] = {'L', 'N', 'A', 'T'};
    Utils::getSecureRandom(marker + 4, 8);
    const int marked = (int)::sendto(fd, reinterpret_cast<const char*>(marker), sizeof(marker), 0,
        reinterpret_cast<const sockaddr*>(&root), sizeof(root));
    log("root_marker", (uint64_t)fd, local, root, hex(marker + 4, 8), "", marked);
    const char* ips[] = {"3.22.142.132", "3.22.142.132", "3.135.212.85", "74.125.250.129", "162.159.207.0"};
    const unsigned short ports[] = {3478, 3479, 3478, 19302, 3478};
    for (unsigned i = 0; i < 5; ++i) {
        unsigned char packet[20] = {0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42};
        Utils::getSecureRandom(packet + 8, 12);
        const std::string tx = hex(packet + 8, 12);
        sockaddr_in remote = {}; remote.sin_family = AF_INET;
        remote.sin_addr.s_addr = ipv4(ips[i]); remote.sin_port = htons(ports[i]);
        s.pending.emplace(tx, Pending{(uint64_t)fd, now, local, remote, false});
        const int result = (int)::sendto(fd, reinterpret_cast<const char*>(packet), sizeof(packet), 0,
            reinterpret_cast<const sockaddr*>(&remote), sizeof(remote));
        log("stun_send", (uint64_t)fd, local, remote, tx, "", result);
    }
}
inline bool consume(ZT_PHY_SOCKFD_TYPE fd, const sockaddr* localAddress,
                    const sockaddr* from, const void* payload, unsigned long length) {
    if (!enabled() || length < 20 || localAddress->sa_family != AF_INET || from->sa_family != AF_INET) return false;
    const unsigned char* b = reinterpret_cast<const unsigned char*>(payload);
    if (b[4] != 0x21 || b[5] != 0x12 || b[6] != 0xa4 || b[7] != 0x42) return false;
    State& s = state(); std::lock_guard<std::mutex> lock(s.mutex);
    const std::string tx = hex(b + 8, 12);
    auto it = s.pending.find(tx);
    if (it == s.pending.end()) return false;
    Pending& p = it->second;
    const sockaddr_in& local = *reinterpret_cast<const sockaddr_in*>(localAddress);
    const sockaddr_in& remote = *reinterpret_cast<const sockaddr_in*>(from);
    if (p.fd != (uint64_t)fd || p.local.sin_port != local.sin_port || p.local.sin_addr.s_addr != local.sin_addr.s_addr ||
        p.remote.sin_port != remote.sin_port || p.remote.sin_addr.s_addr != remote.sin_addr.s_addr) return false;
    if (p.complete) return true;
    p.complete = true;
    const unsigned bodyLength = (unsigned(b[2]) << 8) | b[3];
    const uint64_t now = nowMs();
    if (bodyLength + 20 != length || now < p.sent || now - p.sent > 30000 || b[0] != 1 || b[1] != 1) {
        log("stun_rejected", p.fd, local, remote, tx, "", -1); return true;
    }
    std::string mapped;
    for (unsigned i = 20; i + 4 <= length;) {
        const unsigned type = (unsigned(b[i]) << 8) | b[i+1];
        const unsigned n = (unsigned(b[i+2]) << 8) | b[i+3];
        const unsigned start = i + 4; i += 4 + ((n + 3) & ~3U);
        if (start + n > length) { log("stun_rejected", p.fd, local, remote, tx, "", -2); return true; }
        if (type == 0x20 && n == 8 && b[start+1] == 1) {
            sockaddr_in observed = {}; observed.sin_family = AF_INET;
            observed.sin_port = htons((((unsigned)b[start+2] << 8) | b[start+3]) ^ 0x2112);
            unsigned char* ip = reinterpret_cast<unsigned char*>(&observed.sin_addr.s_addr);
            for (unsigned j = 0; j < 4; ++j) ip[j] = b[start+4+j] ^ b[4+j];
            mapped = endpoint(observed);
        }
    }
    log(mapped.empty() ? "stun_no_mapping" : "stun_response", p.fd, local, remote, tx, mapped, 0, now-p.sent);
    return true;
}
} } // namespace ZeroTier::LinkSocketStun
