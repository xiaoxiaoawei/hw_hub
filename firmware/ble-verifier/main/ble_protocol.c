#include "ble_protocol.h"
#include <string.h>

static const uint8_t service_le[16] = {
    0x4b,0x91,0x31,0xc3,0xc9,0xc5,0xcc,0x8f,
    0x9e,0x45,0xb5,0x1f,0x01,0xc2,0xaf,0x4f
};

static uint8_t ascii_lower(uint8_t value) {
    return value >= 'A' && value <= 'Z' ? (uint8_t)(value + ('a' - 'A')) : value;
}

static bool lumysi_name(const uint8_t *name, size_t length) {
    static const char wanted[] = "lumysi";
    if (!length || length >= BV_NAME_SIZE) return false;
    for (size_t i = 0; i < length; ++i) {
        if (name[i] < 0x20 || name[i] > 0x7e) return false;
    }
    for (size_t offset = 0; offset + sizeof(wanted) - 1 <= length; ++offset) {
        size_t i = 0;
        while (i < sizeof(wanted) - 1 && ascii_lower(name[offset + i]) == (uint8_t)wanted[i]) ++i;
        if (i == sizeof(wanted) - 1) return true;
    }
    return false;
}

static bool service_match(const uint8_t *value, size_t count) {
    if (!count || count % sizeof(service_le)) return false;
    for (size_t i = 0; i < count; i += sizeof(service_le)) {
        if (!memcmp(value + i, service_le, sizeof(service_le))) return true;
    }
    return false;
}

static bool diagnostic_prefix(const uint8_t *value, size_t count) {
    return count >= 3 && value[0] == 0xff && value[1] == 0xff && value[2] == 0x4c;
}

bool bv_packet_interesting(const uint8_t *data, size_t length, bool response) {
    if (!data || length > BV_MAX_AD_BYTES) return false;
    for (size_t offset = 0; offset < length;) {
        unsigned size = data[offset++];
        if (!size) return false;
        size_t available = length - offset;
        if (!available) return false;
        uint8_t type = data[offset];
        const uint8_t *value = data + offset + 1;
        /* Keep a recognizable malformed diagnostic for fail-closed parsing. */
        size_t present_count = size > available ? available - 1 : size - 1;
        if (type == 0xff && diagnostic_prefix(value, present_count)) return true;
        if (size > available) return false;
        size_t count = size - 1;
        if (!response && (type == 0x06 || type == 0x07) && service_match(value, count)) return true;
        if ((type == 0x08 || type == 0x09) && lumysi_name(value, count)) return true;
        offset += size;
    }
    return false;
}

static uint16_t be16(const uint8_t *bytes) {
    return (uint16_t)(((uint16_t)bytes[0] << 8) | bytes[1]);
}
static uint32_t be32(const uint8_t *bytes) {
    return ((uint32_t)bytes[0] << 24) | ((uint32_t)bytes[1] << 16) |
        ((uint32_t)bytes[2] << 8) | bytes[3];
}

static void observe(bv_evidence_t *e, const uint8_t *data, size_t length,
                    bool response, bool *name_match) {
    if ((!data && length) || length > BV_MAX_AD_BYTES) { e->malformed = true; return; }
    for (size_t offset = 0; offset < length;) {
        unsigned size = data[offset++];
        if (!size) {
            for (; offset < length; ++offset) {
                if (data[offset]) { e->malformed = true; break; }
            }
            return;
        }
        if (size > length - offset) { e->malformed = true; return; }
        uint8_t type = data[offset];
        const uint8_t *value = data + offset + 1;
        size_t count = size - 1;
        if (type == 0x06 || type == 0x07) {
            if (!count || count % 16) e->malformed = true;
            else if (!response && service_match(value, count)) e->service_seen = true;
        } else if (type == 0x08 || type == 0x09) {
            bool valid = lumysi_name(value, count);
            if (valid && name_match) *name_match = true;
            if (response && type == 0x09) {
                if (!valid) e->invalid_name = true;
                else {
                    char name[BV_NAME_SIZE] = {0};
                    memcpy(name, value, count);
                    if (e->name_seen && strcmp(name, e->name)) e->conflicting = true;
                    else { memcpy(e->name, name, sizeof(name)); e->name_seen = true; }
                }
            }
        } else if (response && type == 0xff) {
            /* Every FF AD must have the documented shape, including extras. */
            if (count != 12 || !diagnostic_prefix(value, count)) e->malformed = true;
            else {
                uint32_t faults = be32(value + 3);
                uint8_t major = value[7];
                uint16_t minor = be16(value + 8);
                if (e->diagnostic_seen && (e->fault_bits != faults || e->major != major || e->minor != minor)) {
                    e->conflicting = true;
                } else {
                    e->diagnostic_seen = true;
                    e->fault_bits = faults;
                    e->major = major;
                    e->minor = minor;
                }
                e->battery_mv = be16(value + 10);
            }
        }
        offset += size;
    }
}

void bv_observe(bv_evidence_t *e, const uint8_t *data, size_t length, bool response) {
    if (e) observe(e, data, length, response, NULL);
}

bool bv_passes(const bv_evidence_t *e) {
    return e && e->service_seen && e->name_seen && e->diagnostic_seen &&
        !e->malformed && !e->invalid_name && !e->conflicting &&
        e->fault_bits == 0 && e->major == 1 && e->minor == 188;
}

const char *bv_failure_reason(const bv_evidence_t *e, bool observed) {
    if (!observed || !e) return "lumysi_device_not_seen";
    if (e->conflicting) return "conflicting_packets";
    if (e->malformed) return "malformed_advertising_data";
    if (e->invalid_name) return "invalid_complete_name";
    if (!e->name_seen) return "missing_complete_name";
    if (!e->service_seen) return "required_service_missing";
    if (!e->diagnostic_seen) return "diagnostic_scan_response_missing";
    if (e->fault_bits) return "nonzero_fault_bits";
    if (e->major != 1 || e->minor != 188) return "firmware_version_mismatch";
    return "incomplete_scan";
}

void bv_scan_reset(bv_scan_t *scan) {
    if (scan) memset(scan, 0, sizeof(*scan));
}

static bool complete_snapshot(const bv_evidence_t *e, bool response) {
    if (e->malformed || e->invalid_name || e->conflicting) return false;
    return response ? e->name_seen && e->diagnostic_seen : e->service_seen;
}

void bv_scan_observe(bv_scan_t *scan, const uint8_t address[6], uint8_t address_type,
                     const uint8_t *data, size_t length, bool response) {
    if (!scan || !address) return;
    bv_candidate_t *candidate = NULL, *free_slot = NULL;
    for (size_t i = 0; i < BV_MAX_CANDIDATES; ++i) {
        bv_candidate_t *entry = &scan->candidates[i];
        if (!entry->used) { if (!free_slot) free_slot = entry; }
        else if (entry->address_type == address_type && !memcmp(entry->address, address, 6)) {
            candidate = entry;
            break;
        }
    }
    if (!candidate) {
        if (!bv_packet_interesting(data, length, response)) return;
        if (!free_slot) { scan->overflow = true; return; }
        candidate = free_slot;
        memset(candidate, 0, sizeof(*candidate));
        candidate->used = true;
        memcpy(candidate->address, address, 6);
        candidate->address_type = address_type;
    }
    observe(&candidate->evidence, data, length, response, &candidate->named_match);
    uint8_t *raw = response ? candidate->scan_rsp : candidate->adv;
    size_t *raw_length = response ? &candidate->scan_rsp_len : &candidate->adv_len;
    bv_evidence_t previous = {0}, incoming = {0};
    bv_observe(&previous, raw, *raw_length, response);
    bv_observe(&incoming, data, length, response);
    /* Keep the last complete, actually received packet instead of erasing it
       with a valid flags/name/diagnostic-only follow-up. All reports still
       update sticky errors above; malformed/conflicting follow-ups retain the
       old fail-closed behavior. No fields or raw bytes are spliced together. */
    if (complete_snapshot(&previous, response) && !complete_snapshot(&incoming, response) &&
        !candidate->evidence.malformed && !candidate->evidence.invalid_name &&
        !candidate->evidence.conflicting) {
        /* Voltage may vary between advertisements. Report the exact voltage
           in the selected complete scan response, not a later partial one. */
        if (response) candidate->evidence.battery_mv = previous.battery_mv;
        return;
    }
    memset(raw, 0, BV_MAX_AD_BYTES);
    *raw_length = 0;
    if (length <= BV_MAX_AD_BYTES && (data || !length)) {
        if (length) memcpy(raw, data, length);
        *raw_length = length;
    }
}

size_t bv_scan_matching_count(const bv_scan_t *scan) {
    size_t count = 0;
    if (scan) for (size_t i = 0; i < BV_MAX_CANDIDATES; ++i) {
        count += scan->candidates[i].used && scan->candidates[i].named_match;
    }
    return count;
}

const bv_candidate_t *bv_scan_single(const bv_scan_t *scan) {
    if (!scan || scan->overflow || bv_scan_matching_count(scan) != 1) return NULL;
    for (size_t i = 0; i < BV_MAX_CANDIDATES; ++i) {
        if (scan->candidates[i].used && scan->candidates[i].named_match) return &scan->candidates[i];
    }
    return NULL;
}

bool bv_candidate_passes(const bv_candidate_t *candidate) {
    if (!candidate || !candidate->used || !candidate->named_match ||
        candidate->adv_len > BV_MAX_AD_BYTES || candidate->scan_rsp_len > BV_MAX_AD_BYTES ||
        !bv_passes(&candidate->evidence)) return false;
    bv_evidence_t raw = {0};
    bv_observe(&raw, candidate->adv, candidate->adv_len, false);
    bv_observe(&raw, candidate->scan_rsp, candidate->scan_rsp_len, true);
    return bv_passes(&raw) && !strcmp(raw.name, candidate->evidence.name) &&
        raw.fault_bits == candidate->evidence.fault_bits &&
        raw.major == candidate->evidence.major && raw.minor == candidate->evidence.minor &&
        raw.battery_mv == candidate->evidence.battery_mv;
}
