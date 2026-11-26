#include "ble_protocol.h"
#include "station_feedback.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

/* Synthetic packets/addresses only; these are not captured customer records. */
static const uint8_t adv[] = {
    2,1,6,17,6,0x4b,0x91,0x31,0xc3,0xc9,0xc5,0xcc,0x8f,
    0x9e,0x45,0xb5,0x1f,1,0xc2,0xaf,0x4f
};
static const uint8_t address_a[6] = {0x00,0x11,0x22,0xaa,0xbb,0xcc};
static const uint8_t address_b[6] = {0x00,0x11,0x22,0xaa,0xbb,0xdd};

static size_t name_packet(uint8_t *out, const char *name, uint8_t type) {
    size_t count = strlen(name);
    assert(count <= BV_MAX_AD_BYTES - 2);
    out[0] = (uint8_t)(count + 1);
    out[1] = type;
    memcpy(out + 2, name, count);
    return count + 2;
}

static size_t diagnostic(uint8_t *out, uint32_t faults, uint8_t major,
                         uint16_t minor, uint16_t battery) {
    const uint8_t prefix[] = {13,0xff,0xff,0xff,0x4c};
    memcpy(out, prefix, sizeof(prefix));
    out[5] = (uint8_t)(faults >> 24); out[6] = (uint8_t)(faults >> 16);
    out[7] = (uint8_t)(faults >> 8); out[8] = (uint8_t)faults;
    out[9] = major;
    out[10] = (uint8_t)(minor >> 8); out[11] = (uint8_t)minor;
    out[12] = (uint8_t)(battery >> 8); out[13] = (uint8_t)battery;
    return 14;
}

static size_t response(uint8_t *out, const char *name, uint32_t faults,
                        uint8_t major, uint16_t minor, uint16_t battery) {
    size_t length = diagnostic(out, faults, major, minor, battery);
    assert(strlen(name) <= BV_MAX_AD_BYTES - length - 2);
    return length + name_packet(out + length, name, 9);
}

static bv_evidence_t complete(const char *name, uint16_t battery) {
    uint8_t data[BV_MAX_AD_BYTES];
    size_t length = response(data, name, 0, 1, 188, battery);
    bv_evidence_t evidence = {0};
    bv_observe(&evidence, data, length, true);
    assert(!bv_passes(&evidence));
    bv_observe(&evidence, adv, sizeof(adv), false);
    assert(bv_passes(&evidence));
    return evidence;
}

static const bv_candidate_t *find(const bv_scan_t *scan, const uint8_t address[6], uint8_t type) {
    for (size_t i = 0; i < BV_MAX_CANDIDATES; ++i) {
        const bv_candidate_t *entry = &scan->candidates[i];
        if (entry->used && entry->address_type == type && !memcmp(entry->address, address, 6)) return entry;
    }
    return NULL;
}

static size_t used(const bv_scan_t *scan) {
    size_t count = 0;
    for (size_t i = 0; i < BV_MAX_CANDIDATES; ++i) count += scan->candidates[i].used;
    return count;
}

static void parser_uses_substring_case_insensitive_complete_ascii_names(void) {
    const char *names[] = {"Lumysi", "LUMYSI", "lumysi", "xLuMySi-y", "pre Lumysi post"};
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); ++i) {
        bv_evidence_t evidence = complete(names[i], 65535);
        assert(!strcmp(evidence.name, names[i]));
        assert(evidence.battery_mv == 65535);
    }
    const char *bad_names[] = {"", "Lumy", "Other-Lumys", "       "};
    for (size_t i = 0; i < sizeof(bad_names) / sizeof(bad_names[0]); ++i) {
        uint8_t data[BV_MAX_AD_BYTES];
        size_t length = response(data, bad_names[i], 0, 1, 188, 1234);
        bv_evidence_t evidence = {0};
        bv_observe(&evidence, adv, sizeof(adv), false);
        bv_observe(&evidence, data, length, true);
        assert(!bv_passes(&evidence) && evidence.invalid_name);
    }
    for (unsigned bad = 0; bad < 3; ++bad) {
        uint8_t data[BV_MAX_AD_BYTES];
        size_t length = response(data, "Lumysi-x", 0, 1, 188, 0);
        data[length - 1] = bad == 0 ? 0 : bad == 1 ? '\n' : 0x80;
        bv_evidence_t evidence = {0};
        bv_observe(&evidence, data, length, true);
        assert(evidence.invalid_name && !evidence.name_seen);
    }
    /* A shortened name is useful discovery interest, not complete-name proof. */
    uint8_t short_name[BV_MAX_AD_BYTES];
    size_t length = name_packet(short_name, "Lumysi", 8);
    bv_evidence_t evidence = {0};
    bv_observe(&evidence, short_name, length, true);
    assert(bv_packet_interesting(short_name, length, true));
    assert(!evidence.name_seen);
}

static void parser_checks_exact_service_full_fault_word_and_version_but_not_battery(void) {
    uint8_t data[BV_MAX_AD_BYTES];
    bv_evidence_t evidence = complete("Lumysi", 0);
    size_t length = response(data, "Lumysi", 0, 1, 188, 65535);
    bv_observe(&evidence, data, length, true);
    assert(bv_passes(&evidence) && evidence.battery_mv == 65535 && !evidence.conflicting);
    /* This complete protocol has no UID field; no identifier is fabricated. */
    assert(length == 22);
    const uint32_t faults[] = {1,0x100,0x10000,0x80000000};
    for (size_t i = 0; i < sizeof(faults) / sizeof(faults[0]); ++i) {
        evidence = (bv_evidence_t){0};
        length = response(data, "Lumysi", faults[i], 1, 188, 3000);
        bv_observe(&evidence, adv, sizeof(adv), false);
        bv_observe(&evidence, data, length, true);
        assert(!bv_passes(&evidence) && evidence.fault_bits == faults[i]);
        assert(!strcmp(bv_failure_reason(&evidence, true), "nonzero_fault_bits"));
    }
    const uint16_t versions[] = {187,189,65535};
    for (size_t i = 0; i < sizeof(versions) / sizeof(versions[0]); ++i) {
        evidence = (bv_evidence_t){0};
        length = response(data, "Lumysi", 0, 1, versions[i], 0);
        bv_observe(&evidence, adv, sizeof(adv), false);
        bv_observe(&evidence, data, length, true);
        assert(!bv_passes(&evidence));
        assert(!strcmp(bv_failure_reason(&evidence, true), "firmware_version_mismatch"));
    }
    evidence = (bv_evidence_t){0};
    length = response(data, "Lumysi", 0, 2, 188, 0);
    bv_observe(&evidence, adv, sizeof(adv), false);
    bv_observe(&evidence, data, length, true);
    assert(!bv_passes(&evidence));
    uint8_t wrong_service[sizeof(adv)];
    memcpy(wrong_service, adv, sizeof(adv)); wrong_service[6] ^= 1;
    evidence = (bv_evidence_t){0};
    length = response(data, "Lumysi", 0, 1, 188, 0);
    bv_observe(&evidence, wrong_service, sizeof(wrong_service), false);
    bv_observe(&evidence, data, length, true);
    assert(!bv_passes(&evidence) && !evidence.service_seen);
    evidence = (bv_evidence_t){0};
    bv_observe(&evidence, adv, sizeof(adv), true);
    bv_observe(&evidence, data, length, false);
    assert(!bv_passes(&evidence));
    evidence = complete("Lumysi", 0);
    length = response(data, "Lumysi", 0, 1, 187, 0);
    bv_observe(&evidence, data, length, true);
    assert(evidence.conflicting && !bv_passes(&evidence));
    assert(!strcmp(bv_failure_reason(NULL, false), "lumysi_device_not_seen"));
}

static void parser_rejects_every_bad_manufacturer_ad_and_nonzero_after_padding(void) {
    for (unsigned change = 0; change < 5; ++change) {
        uint8_t data[BV_MAX_AD_BYTES];
        size_t length = response(data, "Lumysi", 0, 1, 188, 2922);
        if (change == 0) data[2] = 0xfe;
        if (change == 1) data[3] = 0xfe;
        if (change == 2) data[4] = 0x4d;
        if (change == 3) { data[0] = 12; memmove(data + 13, data + 14, length - 14); --length; }
        if (change == 4) { memmove(data + 15, data + 14, length - 14); data[14] = 0; data[0] = 14; ++length; }
        bv_evidence_t evidence = {0};
        bv_observe(&evidence, data, length, true);
        assert(evidence.malformed && !bv_passes(&evidence));
    }
    uint8_t data[BV_MAX_AD_BYTES] = {0};
    size_t length = response(data, "Lumysi", 0, 1, 188, 2922);
    data[length++] = 2; data[length++] = 0xff; data[length++] = 0x42;
    bv_evidence_t evidence = {0};
    bv_observe(&evidence, data, length, true);
    assert(evidence.malformed); /* An extra unrelated FF may not be ignored. */
    memset(data, 0, sizeof(data));
    length = response(data, "Lumysi", 0, 1, 188, 2922);
    evidence = (bv_evidence_t){0};
    bv_observe(&evidence, adv, sizeof(adv), false);
    bv_observe(&evidence, data, sizeof(data), true);
    assert(bv_passes(&evidence));
    data[30] = 1;
    evidence = (bv_evidence_t){0};
    bv_observe(&evidence, adv, sizeof(adv), false);
    bv_observe(&evidence, data, sizeof(data), true);
    assert(evidence.malformed && !bv_passes(&evidence));
    const uint8_t truncated[] = {13,0xff,0xff,0xff,0x4c};
    assert(bv_packet_interesting(truncated, sizeof(truncated), true));
    evidence = (bv_evidence_t){0};
    bv_observe(&evidence, truncated, sizeof(truncated), true);
    assert(evidence.malformed);
    uint8_t duplicates[28];
    diagnostic(duplicates, 0, 1, 188, 1);
    diagnostic(duplicates + 14, 0, 1, 188, 2);
    evidence = complete("Lumysi", 0);
    bv_observe(&evidence, duplicates, sizeof(duplicates), true);
    assert(bv_passes(&evidence) && evidence.battery_mv == 2);
    duplicates[14 + 8] = 1;
    bv_observe(&evidence, duplicates, sizeof(duplicates), true);
    assert(evidence.conflicting && !bv_passes(&evidence));
}

static void radio_prefilter_and_capacity_ignore_unrelated_reports_but_fail_closed_on_relevant_overflow(void) {
    bv_scan_t scan;
    bv_scan_reset(&scan);
    const uint8_t unrelated[] = {2,1,6,2,0xff,0x42};
    assert(!bv_packet_interesting(unrelated, sizeof(unrelated), false));
    assert(bv_packet_interesting(adv, sizeof(adv), false));
    assert(!bv_packet_interesting(adv, sizeof(adv), true));
    assert(!bv_packet_interesting(NULL, 1, false));
    uint8_t oversized[32] = {0};
    assert(!bv_packet_interesting(oversized, sizeof(oversized), false));
    for (unsigned i = 0; i < 200; ++i) {
        uint8_t address[6] = {1,2,3,4,5,(uint8_t)i};
        bv_scan_observe(&scan, address, 0, unrelated, sizeof(unrelated), false);
    }
    assert(!used(&scan) && !scan.overflow && !bv_scan_single(&scan));
    for (unsigned i = 0; i < BV_MAX_CANDIDATES; ++i) {
        uint8_t address[6] = {1,2,3,4,5,(uint8_t)i};
        bv_scan_observe(&scan, address, 0, adv, sizeof(adv), false);
    }
    assert(used(&scan) == BV_MAX_CANDIDATES && !scan.overflow);
    uint8_t data[BV_MAX_AD_BYTES];
    size_t length = response(data, "Lumysi", 0, 1, 188, 0);
    const uint8_t first[6] = {1,2,3,4,5,0};
    bv_scan_observe(&scan, first, 0, data, length, true);
    assert(bv_scan_matching_count(&scan) == 1 && bv_candidate_passes(bv_scan_single(&scan)));
    bv_scan_observe(&scan, address_b, 0, unrelated, sizeof(unrelated), false);
    assert(!scan.overflow);
    const uint8_t diagnostic_only[] = {4,0xff,0xff,0xff,0x4c};
    bv_scan_observe(&scan, address_b, 0, diagnostic_only, sizeof(diagnostic_only), true);
    assert(scan.overflow && !bv_scan_single(&scan));
    bv_scan_reset(&scan);
    assert(!used(&scan) && !scan.overflow && !bv_scan_matching_count(&scan));
}

static void candidates_keep_rsp_first_raw_snapshot_and_informational_battery(void) {
    bv_scan_t scan;
    bv_scan_reset(&scan);
    uint8_t data[BV_MAX_AD_BYTES];
    size_t length = response(data, "xLuMySi-y", 0, 1, 188, 65535);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(bv_scan_matching_count(&scan) == 1 && !bv_candidate_passes(bv_scan_single(&scan)));
    bv_scan_observe(&scan, address_a, 0, adv, sizeof(adv), false);
    const bv_candidate_t *candidate = bv_scan_single(&scan);
    assert(bv_candidate_passes(candidate) && candidate->evidence.battery_mv == 65535);
    assert(candidate->address[0] == 0 && candidate->address[5] == 0xcc);
    assert(candidate->adv_len == sizeof(adv) && !memcmp(candidate->adv, adv, sizeof(adv)));
    assert(candidate->scan_rsp_len == length && !memcmp(candidate->scan_rsp, data, length));
    length = response(data, "xLuMySi-y", 0, 1, 188, 1);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    candidate = bv_scan_single(&scan);
    assert(bv_candidate_passes(candidate) && candidate->evidence.battery_mv == 1);
    bv_candidate_t inconsistent = *candidate;
    inconsistent.evidence.battery_mv = 2;
    assert(!bv_candidate_passes(&inconsistent));
    bv_scan_observe(&scan, address_a, 0, NULL, 0, true);
    assert(bv_passes(&candidate->evidence) && !bv_candidate_passes(candidate));
}

static void packet_fragments_cannot_fabricate_complete_raw_pair(void) {
    bv_scan_t scan;
    bv_scan_reset(&scan);
    uint8_t data[BV_MAX_AD_BYTES];
    bv_scan_observe(&scan, address_a, 0, adv, sizeof(adv), false);
    size_t length = diagnostic(data, 0, 1, 188, 1234);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(!bv_scan_matching_count(&scan));
    length = name_packet(data, "Lumysi", 9);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    const bv_candidate_t *candidate = bv_scan_single(&scan);
    assert(candidate && bv_passes(&candidate->evidence));
    assert(!bv_candidate_passes(candidate)); /* The actual latest RSP has no diagnostics. */
    length = response(data, "Lumysi", 0, 1, 188, 999);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(bv_candidate_passes(candidate));
    length = diagnostic(data, 0, 1, 188, 1000);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(bv_passes(&candidate->evidence) && !bv_candidate_passes(candidate));
    length = response(data, "Lumysi", 0, 1, 188, 1001);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(bv_candidate_passes(candidate));
    const uint8_t flags_only[] = {2,1,6};
    bv_scan_observe(&scan, address_a, 0, flags_only, sizeof(flags_only), false);
    assert(bv_passes(&candidate->evidence) && !bv_candidate_passes(candidate));
}

static void all_named_candidates_count_even_with_faults_missing_service_or_different_address_types(void) {
    bv_scan_t scan;
    bv_scan_reset(&scan);
    uint8_t data[BV_MAX_AD_BYTES];
    size_t length = response(data, "Lumysi", 0, 1, 188, 0);
    bv_scan_observe(&scan, address_a, 0, adv, sizeof(adv), false);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    assert(bv_candidate_passes(bv_scan_single(&scan)));
    length = response(data, "badLUMYSI", 0x80000000, 2, 187, 0);
    bv_scan_observe(&scan, address_b, 0, data, length, true);
    assert(bv_scan_matching_count(&scan) == 2 && !bv_scan_single(&scan));
    const bv_candidate_t *bad = find(&scan, address_b, 0);
    assert(bad && !bad->evidence.service_seen && !bv_candidate_passes(bad));
    bv_scan_reset(&scan);
    length = response(data, "Lumysi", 0, 1, 188, 0);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    bv_scan_observe(&scan, address_a, 1, data, length, true);
    assert(used(&scan) == 2 && bv_scan_matching_count(&scan) == 2 && !bv_scan_single(&scan));
    bv_scan_reset(&scan);
    length = name_packet(data, "prefixLumysi", 9);
    bv_scan_observe(&scan, address_a, 0, data, length, false);
    assert(bv_scan_matching_count(&scan) == 1 && !bv_candidate_passes(bv_scan_single(&scan)));
    length = name_packet(data, "LuMySi", 8);
    bv_scan_observe(&scan, address_b, 0, data, length, true);
    assert(used(&scan) == 2 && bv_scan_matching_count(&scan) == 2 && !bv_scan_single(&scan));
    length = name_packet(data, "LuMySi", 9);
    bv_scan_observe(&scan, address_b, 0, data, length, false);
    assert(bv_scan_matching_count(&scan) == 2 && !bv_scan_single(&scan));
    bv_scan_reset(&scan);
    length = name_packet(data, "LuMySi", 8);
    bv_scan_observe(&scan, address_a, 0, adv, sizeof(adv), false);
    bv_scan_observe(&scan, address_a, 0, data, length, true);
    const bv_candidate_t *short_only = bv_scan_single(&scan);
    assert(short_only && !bv_candidate_passes(short_only));
    assert(!strcmp(bv_failure_reason(&short_only->evidence, true), "missing_complete_name"));
}

static void sound_phases_last_exactly_three_continuous_half_seconds(void) {
    const uint32_t patterns[3][3] = {{3600,4000,4400}, {4400,4000,4400}, {4400,4000,3600}};
    const uint64_t offsets[] = {0,499,500,999,1000,1499};
    assert(BV_SOUND_STEP_MS == 500 && BV_SOUND_TOTAL_MS == 1500);
    for (unsigned kind = BV_SOUND_START; kind <= BV_SOUND_FAIL; ++kind) {
        bv_sound_state_t sound = {0};
        if (kind == BV_SOUND_START) assert(bv_sound_start(&sound, 100));
        else bv_sound_result(&sound, (bv_sound_kind_t)kind, 100);
        for (size_t i = 0; i < sizeof(offsets) / sizeof(offsets[0]); ++i) {
            uint64_t now = 100 + offsets[i];
            assert(bv_sound_tick(&sound, now) == patterns[kind - BV_SOUND_START][offsets[i] / 500]);
            assert(bv_sound_busy(&sound, now));
            assert(!bv_sound_start(&sound, now));
            assert(sound.started_ms == 100 && sound.active == (bv_sound_kind_t)kind);
            if (kind != BV_SOUND_START) {
                bv_sound_result(&sound, kind == BV_SOUND_PASS ? BV_SOUND_FAIL : BV_SOUND_PASS, now);
                assert(sound.active == (bv_sound_kind_t)kind && sound.pending == BV_SOUND_NONE);
            }
        }
        assert(bv_sound_tick(&sound, 1600) == 0);
        assert(!bv_sound_busy(&sound, 1600));
        assert(sound.active == BV_SOUND_NONE && sound.pending == BV_SOUND_NONE);
    }
}

static void immediate_result_waits_for_full_start_then_plays_full_outcome_without_restart(void) {
    bv_sound_state_t sound = {0};
    assert(bv_sound_start(&sound, 100));
    bv_sound_result(&sound, BV_SOUND_FAIL, 100);
    bv_sound_result(&sound, BV_SOUND_PASS, 250); /* Repeated result cannot replace FAIL. */
    assert(sound.active == BV_SOUND_START && sound.pending == BV_SOUND_FAIL && sound.started_ms == 100);
    assert(bv_sound_tick(&sound, 599) == 3600);
    assert(bv_sound_tick(&sound, 600) == 4000);
    assert(bv_sound_tick(&sound, 1100) == 4400);
    assert(bv_sound_tick(&sound, 1599) == 4400 && sound.active == BV_SOUND_START);
    assert(bv_sound_tick(&sound, 1600) == 4400 && sound.active == BV_SOUND_FAIL);
    assert(sound.started_ms == 1600 && sound.pending == BV_SOUND_NONE);
    assert(!bv_sound_start(&sound, 1600));
    bv_sound_result(&sound, BV_SOUND_PASS, 1700);
    assert(sound.active == BV_SOUND_FAIL && sound.started_ms == 1600);
    assert(bv_sound_tick(&sound, 2099) == 4400);
    assert(bv_sound_tick(&sound, 2100) == 4000);
    assert(bv_sound_tick(&sound, 2600) == 3600);
    assert(bv_sound_tick(&sound, 3099) == 3600 && !bv_sound_start(&sound, 3099));
    assert(bv_sound_tick(&sound, 3100) == 0 && !bv_sound_busy(&sound, 3100));
    assert(bv_sound_start(&sound, 3100));
    assert(bv_sound_tick(&sound, 3100) == 3600 && sound.started_ms == 3100);
}

static void sound_delayed_poll_uses_scheduled_boundaries_and_idle_results_start_now(void) {
    bv_sound_state_t sound = {0};
    assert(!bv_sound_busy(&sound, 0) && bv_sound_tick(&sound, 0) == 0);
    bv_sound_result(&sound, BV_SOUND_NONE, 0);
    bv_sound_result(&sound, BV_SOUND_START, 0);
    assert(!bv_sound_busy(&sound, 0));
    assert(bv_sound_start(&sound, 10));
    bv_sound_result(&sound, BV_SOUND_PASS, 11);
    assert(bv_sound_tick(&sound, 2010) == 4000);
    assert(sound.active == BV_SOUND_PASS && sound.started_ms == 1510);
    assert(bv_sound_tick(&sound, 3010) == 0 && !bv_sound_busy(&sound, 3010));
    assert(bv_sound_start(&sound, 4000));
    bv_sound_result(&sound, BV_SOUND_FAIL, 4001);
    assert(bv_sound_tick(&sound, 10000) == 0 && !bv_sound_busy(&sound, 10000));
    assert(sound.active == BV_SOUND_NONE && sound.pending == BV_SOUND_NONE);
    bv_sound_result(&sound, BV_SOUND_FAIL, 12345);
    assert(sound.started_ms == 12345 && bv_sound_tick(&sound, 12345) == 4400);
    assert(bv_sound_tick(&sound, 13845) == 0);
    assert(!bv_sound_start(NULL, 0) && !bv_sound_busy(NULL, 0) && bv_sound_tick(NULL, 0) == 0);
    bv_sound_result(NULL, BV_SOUND_FAIL, 0);
}

static void timeout_reason_preserves_diagnostic_only_when_complete_buffer_fits(void) {
    char buffer[128];
    assert(!strcmp(bv_timeout_reason(NULL, buffer, sizeof(buffer)), "scan_timeout"));
    assert(!strcmp(bv_timeout_reason("", buffer, sizeof(buffer)), "scan_timeout"));
    assert(!strcmp(bv_timeout_reason("fault", NULL, 100), "scan_timeout"));
    const char *expected = "scan_timeout:nonzero_fault_bits";
    assert(bv_timeout_reason("nonzero_fault_bits", buffer, strlen(expected) + 1) == buffer);
    assert(!strcmp(buffer, expected));
    memset(buffer, 'x', sizeof(buffer));
    assert(!strcmp(bv_timeout_reason("nonzero_fault_bits", buffer, strlen(expected)), "scan_timeout"));
    for (size_t i = 0; i < sizeof(buffer); ++i) assert(buffer[i] == 'x');
    assert(!strcmp(bv_timeout_reason("fault", buffer, 0), "scan_timeout"));
    assert(!strcmp(bv_timeout_reason("fault", buffer, 13), "scan_timeout"));
    assert(bv_timeout_reason("fault", buffer, sizeof(buffer)) == buffer);
    assert(!strcmp(buffer, "scan_timeout:fault"));
    strcpy(buffer, "fault");
    assert(!strcmp(bv_timeout_reason(buffer, buffer, sizeof(buffer)), "scan_timeout:fault"));
}

int main(void) {
    assert(BV_SCAN_MS == 25000);
    parser_uses_substring_case_insensitive_complete_ascii_names();
    parser_checks_exact_service_full_fault_word_and_version_but_not_battery();
    parser_rejects_every_bad_manufacturer_ad_and_nonzero_after_padding();
    radio_prefilter_and_capacity_ignore_unrelated_reports_but_fail_closed_on_relevant_overflow();
    candidates_keep_rsp_first_raw_snapshot_and_informational_battery();
    packet_fragments_cannot_fabricate_complete_raw_pair();
    all_named_candidates_count_even_with_faults_missing_service_or_different_address_types();
    sound_phases_last_exactly_three_continuous_half_seconds();
    immediate_result_waits_for_full_start_then_plays_full_outcome_without_restart();
    sound_delayed_poll_uses_scheduled_boundaries_and_idle_results_start_now();
    timeout_reason_preserves_diagnostic_only_when_complete_buffer_fits();
    puts("ble-verifier portable protocol tests passed");
    return 0;
}
