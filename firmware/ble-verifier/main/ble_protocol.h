#ifndef BLE_VERIFIER_PROTOCOL_H
#define BLE_VERIFIER_PROTOCOL_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define BV_SCAN_MS 60000
#define BV_NAME_SIZE 32
#define BV_MAX_AD_BYTES 31
#define BV_MAX_CANDIDATES 32

typedef struct {
    bool service_seen, name_seen, diagnostic_seen;
    bool malformed, invalid_name, conflicting;
    char name[BV_NAME_SIZE];
    uint32_t fault_bits;
    uint8_t major;
    uint16_t minor, battery_mv;
} bv_evidence_t;

typedef struct {
    bool used;
    /* Conventional display order, first displayed octet first. */
    uint8_t address[6];
    uint8_t address_type;
    bv_evidence_t evidence;
    uint8_t adv[BV_MAX_AD_BYTES];
    size_t adv_len;
    uint8_t scan_rsp[BV_MAX_AD_BYTES];
    size_t scan_rsp_len;
    /* Sticky: a printable ASCII complete/short Lumysi name was seen in either PDU. */
    bool named_match;
} bv_candidate_t;

typedef struct {
    bv_candidate_t candidates[BV_MAX_CANDIDATES];
    /* An untracked relevant address could make a single-device result unsafe. */
    bool overflow;
} bv_scan_t;

/* Bounded radio prefilter. Short names/diagnostic prefixes are interesting
 * enough to observe, but cannot by themselves certify a named passing device. */
bool bv_packet_interesting(const uint8_t *data, size_t length, bool scan_response);
/* Service is accepted only in ADV; complete name and exact diagnostics only
 * in SCAN_RSP. Errors/conflicts are sticky; battery changes are informational. */
void bv_observe(bv_evidence_t *evidence, const uint8_t *data, size_t length,
                bool scan_response);
bool bv_passes(const bv_evidence_t *evidence);
const char *bv_failure_reason(const bv_evidence_t *evidence, bool observed);

void bv_scan_reset(bv_scan_t *scan);
void bv_scan_observe(bv_scan_t *scan, const uint8_t address[6], uint8_t address_type,
                     const uint8_t *data, size_t length, bool scan_response);
/* Count complete/short named Lumysi candidates, including bad diagnostics. */
size_t bv_scan_matching_count(const bv_scan_t *scan);
/* NULL on ambiguity, no named match, or capacity overflow. */
const bv_candidate_t *bv_scan_single(const bv_scan_t *scan);
/* Independently reparse the selected unmodified actual ADV/RSP pair. A valid
 * shorter follow-up does not erase an already complete real packet. Sticky
 * errors still reject; separate RSP fragments cannot fabricate raw proof. */
bool bv_candidate_passes(const bv_candidate_t *candidate);
#endif
