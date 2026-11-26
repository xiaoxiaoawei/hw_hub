#ifndef BLE_VERIFIER_PROTOCOL_H
#define BLE_VERIFIER_PROTOCOL_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define BV_VERSION "0.1.0"
#define BV_SCAN_MS 45000
#define BV_SESSION_SIZE 65
#define BV_MAC_SIZE 13
#define BV_NAME_SIZE 32

typedef struct {
    char session[BV_SESSION_SIZE];
    char mac[BV_MAC_SIZE];
    /* Conventional display order, first displayed octet first. */
    uint8_t address[6];
} bv_target_t;

typedef struct {
    bool service_seen, name_seen, diagnostic_seen;
    bool malformed, invalid_name, conflicting;
    char name[BV_NAME_SIZE];
    uint32_t fault_bits;
    uint8_t major;
    uint16_t minor, battery_mv;
} bv_evidence_t;

/* Exact ASCII grammar; optional single trailing CR is accepted for CRLF. */
bool bv_parse_target(const char *line, size_t length, bv_target_t *target);
/* Main ADV and SCAN_RSP must belong to the same address AND address type. */
void bv_observe(bv_evidence_t *evidence, const uint8_t *data, size_t length,
                bool scan_response);
bool bv_passes(const bv_evidence_t *evidence);
const char *bv_failure_reason(const bv_evidence_t *evidence, bool observed);
#endif
