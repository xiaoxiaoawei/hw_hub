#include "ble_protocol.h"
#include <string.h>

static const uint8_t service_le[16] = {
    0x4b,0x91,0x31,0xc3,0xc9,0xc5,0xcc,0x8f,
    0x9e,0x45,0xb5,0x1f,0x01,0xc2,0xaf,0x4f
};

static int hex(unsigned char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

bool bv_parse_target(const char *line, size_t length, bv_target_t *target) {
    static const char prefix[] = "BLE_TARGET ";
    if (length && line[length - 1] == '\r') --length;
    if (length != 11 + 64 + 1 + 12 || memcmp(line, prefix, 11) || line[75] != ' ') return false;
    for (size_t i = 11; i < 75; ++i) {
        unsigned char c = (unsigned char)line[i];
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    }
    bv_target_t parsed = {0};
    memcpy(parsed.session, line + 11, 64);
    memcpy(parsed.mac, line + 76, 12);
    for (size_t i = 0; i < 6; ++i) {
        int high = hex((unsigned char)parsed.mac[2 * i]);
        int low = hex((unsigned char)parsed.mac[2 * i + 1]);
        if (high < 0 || low < 0) return false;
        parsed.address[i] = (uint8_t)((high << 4) | low);
    }
    bool nonzero = false, nonff = false;
    for (size_t i = 0; i < 6; ++i) {
        nonzero |= parsed.address[i] != 0;
        nonff |= parsed.address[i] != 0xff;
    }
    if (!nonzero || !nonff) return false;
    *target = parsed;
    return true;
}

static bool printable_name(const uint8_t *s, size_t n) {
    bool suffix = false;
    for (size_t i = 0; i < n; ++i) {
        if (s[i] < 0x20 || s[i] > 0x7e) return false;
        if (i >= 7 && s[i] != ' ') suffix = true;
    }
    return suffix;
}

static uint16_t be16(const uint8_t *bytes) { return ((uint16_t)bytes[0] << 8) | bytes[1]; }
static uint32_t be32(const uint8_t *bytes) {
    return ((uint32_t)bytes[0] << 24) | ((uint32_t)bytes[1] << 16) | ((uint32_t)bytes[2] << 8) | bytes[3];
}

void bv_observe(bv_evidence_t *e, const uint8_t *data, size_t length, bool response) {
    for (size_t offset = 0; offset < length;) {
        unsigned size = data[offset++];
        if (!size) return; /* Permitted zero padding. */
        if (size > length - offset) { e->malformed = true; return; }
        uint8_t type = data[offset];
        const uint8_t *value = data + offset + 1;
        size_t count = size - 1;
        if (!response && (type == 0x06 || type == 0x07)) {
            if (count == 0 || count % 16) e->malformed = true;
            else for (size_t i = 0; i < count; i += 16) {
                if (memcmp(value + i, service_le, sizeof(service_le)) == 0) e->service_seen = true;
            }
        } else if (response && type == 0x09) {
            if (count <= 7 || count >= BV_NAME_SIZE || memcmp(value, "Lumysi-", 7) || !printable_name(value, count)) {
                e->invalid_name = true;
            } else {
                char name[BV_NAME_SIZE] = {0};
                memcpy(name, value, count);
                if (e->name_seen && strcmp(name, e->name)) e->conflicting = true;
                else { memcpy(e->name, name, sizeof(name)); e->name_seen = true; }
            }
        } else if (response && type == 0xff && count >= 3 && value[0] == 0xff && value[1] == 0xff && value[2] == 0x4c) {
            if (count != 12) e->malformed = true;
            else {
                uint32_t faults = be32(value + 3);
                uint8_t major = value[7];
                uint16_t minor = be16(value + 8), battery = be16(value + 10);
                if (e->diagnostic_seen && (e->fault_bits != faults || e->major != major || e->minor != minor)) e->conflicting = true;
                else { e->diagnostic_seen = true; e->fault_bits = faults; e->major = major; e->minor = minor; }
                e->battery_mv = battery; /* Informational: normal variation is not a failure. */
            }
        }
        offset += size;
    }
}

bool bv_passes(const bv_evidence_t *e) {
    return e->service_seen && e->name_seen && e->diagnostic_seen && !e->malformed &&
        !e->invalid_name && !e->conflicting && e->fault_bits == 0 && e->major == 1 && e->minor == 188;
}

const char *bv_failure_reason(const bv_evidence_t *e, bool observed) {
    if (!observed) return "expected_target_not_seen";
    if (e->conflicting) return "conflicting_packets";
    if (e->malformed) return "malformed_advertising_data";
    if (e->invalid_name) return "invalid_complete_name";
    if (!e->service_seen) return "required_service_missing";
    if (!e->name_seen) return "complete_name_missing";
    if (!e->diagnostic_seen) return "diagnostic_scan_response_missing";
    if (e->fault_bits) return "nonzero_fault_bits";
    if (e->major != 1 || e->minor != 188) return "firmware_version_mismatch";
    return "incomplete_scan";
}
