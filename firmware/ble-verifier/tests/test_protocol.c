#include "ble_protocol.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

/* Synthetic fixtures; no captured customer packet or device identifier. */
static const uint8_t adv[] = {2,1,6,17,6,0x4b,0x91,0x31,0xc3,0xc9,0xc5,0xcc,0x8f,0x9e,0x45,0xb5,0x1f,1,0xc2,0xaf,0x4f};
static const uint8_t response[] = {13,0xff,0xff,0xff,0x4c,0,0,0,0,1,0,188,0xff,0xff,12,9,'L','u','m','y','s','i','-','0','0','0','0'};

static bv_evidence_t complete(void) {
    bv_evidence_t e = {0};
    bv_observe(&e, response, sizeof(response), true);
    assert(!bv_passes(&e));
    bv_observe(&e, adv, sizeof(adv), false);
    assert(bv_passes(&e));
    return e;
}

int main(void) {
    bv_target_t target;
    const char *command = "BLE_TARGET 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef 001122AABBCC";
    assert(bv_parse_target(command, strlen(command), &target));
    assert(!strcmp(target.mac, "001122AABBCC"));
    assert(target.address[0] == 0 && target.address[5] == 0xcc);
    char invalid[128]; strcpy(invalid, command); invalid[11] = 'A';
    assert(!bv_parse_target(invalid, strlen(invalid), &target));
    strcpy(invalid, command); invalid[86] = 'c';
    assert(!bv_parse_target(invalid, strlen(invalid), &target));
    assert(!bv_parse_target(command, strlen(command) - 1, &target));
    strcpy(invalid, command); memset(invalid + 76, '0', 12);
    assert(!bv_parse_target(invalid, strlen(invalid), &target));
    strcpy(invalid, command); memset(invalid + 76, 'F', 12);
    assert(!bv_parse_target(invalid, strlen(invalid), &target));
    bv_evidence_t e = complete();
    assert(e.battery_mv == 65535); /* Battery is reported, never a voltage gate. */
    assert(e.major == 1 && e.minor == 188 && e.fault_bits == 0);
    bv_observe(&e, response, sizeof(response), true); assert(bv_passes(&e));
    uint8_t changed[sizeof(response)]; memcpy(changed, response, sizeof(changed));
    changed[12] = 0; changed[13] = 0;
    bv_observe(&e, changed, sizeof(changed), true);
    assert(bv_passes(&e) && e.battery_mv == 0); /* Voltage variation remains informational. */
    memcpy(changed, response, sizeof(changed));
    changed[5] = 0x80; /* Reserved high fault bit must also fail. */
    e = (bv_evidence_t){0}; bv_observe(&e, adv, sizeof(adv), false); bv_observe(&e, changed, sizeof(changed), true);
    assert(!bv_passes(&e) && e.fault_bits == 0x80000000);
    memcpy(changed, response, sizeof(changed)); changed[11] = 187;
    e = (bv_evidence_t){0}; bv_observe(&e, adv, sizeof(adv), false); bv_observe(&e, changed, sizeof(changed), true);
    assert(!bv_passes(&e) && !strcmp(bv_failure_reason(&e, true), "firmware_version_mismatch"));
    e = complete(); bv_observe(&e, changed, sizeof(changed), true); assert(e.conflicting && !bv_passes(&e));
    e = (bv_evidence_t){0}; bv_observe(&e, adv, sizeof(adv), true); bv_observe(&e, response, sizeof(response), false);
    assert(!bv_passes(&e)); /* Cannot invent scan-response diagnostics from ADV. */
    uint8_t short_ad[] = {31,0xff,0xff};
    e = (bv_evidence_t){0}; bv_observe(&e, short_ad, sizeof(short_ad), true); assert(e.malformed);
    uint8_t name_only[] = {8,9,'L','u','m','y','s','i','-'};
    e = (bv_evidence_t){0}; bv_observe(&e, name_only, sizeof(name_only), true); assert(e.invalid_name);
    for (unsigned invalid_suffix = 0; invalid_suffix < 3; ++invalid_suffix) {
        memcpy(changed, response, sizeof(changed));
        if (invalid_suffix == 0) memset(changed + 23, ' ', 4);
        else changed[26] = invalid_suffix == 1 ? 0x80 : '\n';
        e = (bv_evidence_t){0}; bv_observe(&e, changed, sizeof(changed), true); assert(e.invalid_name && !bv_passes(&e));
    }
    assert(!strcmp(bv_failure_reason(&(bv_evidence_t){0}, false), "expected_target_not_seen"));
    puts("ble-verifier portable protocol tests passed");
    return 0;
}
