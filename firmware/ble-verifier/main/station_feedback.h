#ifndef BLE_VERIFIER_STATION_FEEDBACK_H
#define BLE_VERIFIER_STATION_FEEDBACK_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#define BV_SOUND_STEP_MS 500u
#define BV_SOUND_TOTAL_MS 1500u

typedef enum bv_sound_kind {
    BV_SOUND_NONE = 0,
    BV_SOUND_START,
    BV_SOUND_PASS,
    BV_SOUND_FAIL
} bv_sound_kind_t;

typedef struct {
    bv_sound_kind_t active, pending;
    uint64_t started_ms;
} bv_sound_state_t;

/* Pure absolute-time state advancement: no wait, task or hardware operation.
 * A queued outcome is anchored to START's scheduled end, even if polling is
 * late. The subtraction check also avoids overflowing the end timestamp. */
static inline void bv_sound_advance(bv_sound_state_t *state, uint64_t now_ms) {
    if (!state) return;
    while (state->active != BV_SOUND_NONE && now_ms >= state->started_ms &&
           now_ms - state->started_ms >= BV_SOUND_TOTAL_MS) {
        state->started_ms += BV_SOUND_TOTAL_MS;
        state->active = state->pending;
        state->pending = BV_SOUND_NONE;
    }
}

/* Frequencies describe three continuous 500 ms phases. The caller drives
 * the selected frequency at 50% duty, or silences its output for zero. */
static inline uint32_t bv_sound_tick(bv_sound_state_t *state, uint64_t now_ms) {
    static const uint32_t start_hz[3] = {3600, 4000, 4400};
    static const uint32_t pass_hz[3] = {4400, 4000, 4400};
    static const uint32_t fail_hz[3] = {4400, 4000, 3600};
    if (!state) return 0;
    bv_sound_advance(state, now_ms);
    if (state->active == BV_SOUND_NONE) return 0;
    uint64_t elapsed = now_ms >= state->started_ms ? now_ms - state->started_ms : 0;
    size_t phase = (size_t)(elapsed / BV_SOUND_STEP_MS);
    switch (state->active) {
        case BV_SOUND_START: return start_hz[phase];
        case BV_SOUND_PASS: return pass_hz[phase];
        case BV_SOUND_FAIL: return fail_hz[phase];
        default: return 0;
    }
}

static inline bool bv_sound_start(bv_sound_state_t *state, uint64_t now_ms) {
    if (!state) return false;
    bv_sound_advance(state, now_ms);
    if (state->active != BV_SOUND_NONE || state->pending != BV_SOUND_NONE) return false;
    state->active = BV_SOUND_START;
    state->started_ms = now_ms;
    return true;
}

/* A result never cuts START short or changes an already queued/playing result.
 * The station owns the per-scan finished guard; a result received after all
 * sounds have ended is a new idle result, not identifiable as an old duplicate. */
static inline void bv_sound_result(bv_sound_state_t *state, bv_sound_kind_t kind,
                                    uint64_t now_ms) {
    if (!state) return;
    bv_sound_advance(state, now_ms);
    if (kind != BV_SOUND_PASS && kind != BV_SOUND_FAIL) return;
    if (state->active == BV_SOUND_NONE && state->pending == BV_SOUND_NONE) {
        state->active = kind;
        state->started_ms = now_ms;
    } else if (state->active == BV_SOUND_START && state->pending == BV_SOUND_NONE) {
        state->pending = kind;
    }
}

static inline bool bv_sound_busy(bv_sound_state_t *state, uint64_t now_ms) {
    if (!state) return false;
    bv_sound_advance(state, now_ms);
    return state->active != BV_SOUND_NONE || state->pending != BV_SOUND_NONE;
}

/* Always use the returned pointer: insufficient storage returns the literal
 * fallback without partially modifying the caller's buffer. */
static inline const char *bv_timeout_reason(const char *diagnostic, char *buffer,
                                            size_t capacity) {
    static const char fallback[] = "scan_timeout";
    static const char prefix[] = "scan_timeout:";
    const size_t prefix_length = sizeof(prefix) - 1;
    if (!diagnostic || !*diagnostic || !buffer || capacity <= prefix_length) return fallback;
    size_t room = capacity - prefix_length;
    size_t length = 0;
    while (length < room && diagnostic[length]) ++length;
    if (length == room) return fallback;
    /* memmove also supports diagnostic residing in the output buffer. */
    memmove(buffer + prefix_length, diagnostic, length + 1);
    memcpy(buffer, prefix, prefix_length);
    return buffer;
}
#endif
