/* Link packet adapter ABI 1. Default mode remains libzt's internal lwIP.
 * Register only before the first node start (or after external-mode join).
 * Switching a process that already started legacy lwIP into external mode is
 * rejected; restart that process rather than guess at old thread ownership.
 * While offline, callback==NULL and context==NULL selects lwIP.
 * External mode is exclusive: no duplicate lwIP owner of the interface/IP.
 * Callback bytes are borrowed until return. Copy into a bounded queue and
 * return promptly; do not block, throw, or reenter ANY libzt API in callback.
 * Generation changes on start and network configuration changes. Read it after
 * the network configuration event; stale OS-settings completions must fail.
 * Set ready only after
 * the OS interface/settings successfully install; registration is not ready.
 * stop rejects stale runs and waits for callbacks/injections. On timeout keep
 * context alive. Before releasing identity ownership also stop the node and
 * successfully join via zts_packet_adapter_join; PID records are not locks.
 */
typedef void (*zts_packet_frame_callback)(void *context, uint64_t generation,
    uint64_t network_id, uint64_t source_mac, uint64_t destination_mac,
    uint32_t ether_type, uint32_t vlan, const void *payload, uint32_t length);
ZTS_API int ZTCALL zts_init_set_packet_adapter(uint32_t abi, zts_packet_frame_callback callback, void *context);
ZTS_API int ZTCALL zts_packet_adapter_get_generation(uint64_t *generation);
ZTS_API int ZTCALL zts_packet_adapter_set_ready(uint64_t generation, uint64_t network_id, int ready);
ZTS_API int ZTCALL zts_packet_adapter_inject(uint64_t generation, uint64_t network_id,
    uint64_t source_mac, uint64_t destination_mac, uint32_t ether_type,
    uint32_t vlan, const void *payload, uint32_t length);
ZTS_API int ZTCALL zts_packet_adapter_stop(uint64_t generation, uint32_t timeout_ms);
/* External mode only. Request node stop first; waits completion then joins the
 * actual service and event callback threads. Timeout retains all owner state. */
ZTS_API int ZTCALL zts_packet_adapter_join(uint64_t generation, uint32_t timeout_ms);
