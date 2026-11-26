#include "ble_protocol.h"
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "cJSON.h"
#include "driver/gpio.h"
#include "driver/ledc.h"
#include "driver/uart.h"
#include "driver/usb_serial_jtag.h"
#include "esp_err.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "host/ble_hs.h"
#include "host/util/util.h"
#include "nimble/ble.h"
#include "nimble/hci_common.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "nvs_flash.h"

#define BUTTON_PIN GPIO_NUM_7
#define BUZZER_PIN GPIO_NUM_1
#define HOST_READY BIT0
#define REPORT_QUEUE_LENGTH 64

typedef struct {
    uint32_t attempt;
    bool complete, response;
    uint8_t address_type, length;
    uint8_t data[31];
} report_t;
typedef struct { char data[128]; size_t length; bool overflow; } input_line_t;

static QueueHandle_t reports;
static EventGroupHandle_t events;
static uint8_t own_address_type;
/* The following station state belongs exclusively to the I/O task. */
static bv_target_t target;
static char consumed_session[BV_SESSION_SIZE];
static uint64_t target_revision;
static bool armed, ready_pending, busy, finished, observed, type_seen, ambiguous_type;
static uint8_t selected_type;
static uint32_t attempt;
static portMUX_TYPE scan_lock = portMUX_INITIALIZER_UNLOCKED;
static uint32_t active_attempt, completed_attempt, overflow_attempt;
static uint8_t active_address[6];
static bv_evidence_t evidence;
static int64_t deadline_us;

static struct { unsigned pulses; bool on; uint32_t on_ms, off_ms; int64_t deadline; } tone;
static void tone_level(bool on) {
    ledc_set_duty(LEDC_LOW_SPEED_MODE, LEDC_CHANNEL_0, on ? 128 : 0);
    ledc_update_duty(LEDC_LOW_SPEED_MODE, LEDC_CHANNEL_0);
}
static void beep(unsigned count, uint32_t on_ms, uint32_t off_ms) {
    tone.pulses = count; tone.on = true; tone.on_ms = on_ms; tone.off_ms = off_ms;
    tone.deadline = esp_timer_get_time() + (int64_t)on_ms * 1000;
    tone_level(true);
}
static void tick_beep(int64_t now) {
    if (!tone.pulses || now < tone.deadline) return;
    if (tone.on) {
        tone_level(false); tone.on = false;
        if (!--tone.pulses) return;
        tone.deadline = now + (int64_t)tone.off_ms * 1000;
    } else {
        tone_level(true); tone.on = true;
        tone.deadline = now + (int64_t)tone.on_ms * 1000;
    }
}

static void write_line(const char *text) {
    /* Neither transport is used from a BLE callback. Hardware UART TX drains
       independently of a receiver; USB writes have a bounded timeout. */
    uart_write_bytes(UART_NUM_0, text, strlen(text));
    uart_write_bytes(UART_NUM_0, "\n", 1);
    usb_serial_jtag_write_bytes(text, strlen(text), pdMS_TO_TICKS(20));
    usb_serial_jtag_write_bytes("\n", 1, pdMS_TO_TICKS(20));
}
static bool emit(const char *kind, const char *status, const char *reason) {
    cJSON *json = cJSON_CreateObject();
    if (!json) return false;
    bool valid = cJSON_AddStringToObject(json, "event", kind) != NULL;
    valid &= cJSON_AddStringToObject(json, "session", target.session) != NULL;
    valid &= cJSON_AddStringToObject(json, "mac", target.mac) != NULL;
    if (strcmp(kind, "ready")) valid &= cJSON_AddNumberToObject(json, "attempt", attempt) != NULL;
    if (status) {
        valid &= cJSON_AddStringToObject(json, "status", status) != NULL;
        if (observed) valid &= cJSON_AddStringToObject(json, "observed_mac", target.mac) != NULL;
        if (evidence.name_seen) valid &= cJSON_AddStringToObject(json, "name", evidence.name) != NULL;
        if (evidence.diagnostic_seen) {
            char version[16]; snprintf(version, sizeof(version), "%u.%u", evidence.major, evidence.minor);
            valid &= cJSON_AddStringToObject(json, "firmware", version) != NULL;
            valid &= cJSON_AddNumberToObject(json, "fault_bits", evidence.fault_bits) != NULL;
            valid &= cJSON_AddNumberToObject(json, "battery_mv", evidence.battery_mv) != NULL;
        }
        if (reason) valid &= cJSON_AddStringToObject(json, "reason", reason) != NULL;
    }
    if (!valid) { cJSON_Delete(json); return false; }
    char *encoded = cJSON_PrintUnformatted(json);
    cJSON_Delete(json);
    if (!encoded) return false;
    char line[640];
    int length = snprintf(line, sizeof(line), "BLE_VERIFY %s", encoded);
    free(encoded);
    if (length < 0 || (size_t)length >= sizeof(line)) return false;
    write_line(line);
    return true;
}

static void clear_active(uint32_t generation) {
    portENTER_CRITICAL(&scan_lock);
    if (active_attempt == generation) active_attempt = 0;
    if (completed_attempt == generation) completed_attempt = 0;
    if (overflow_attempt == generation) overflow_attempt = 0;
    portEXIT_CRITICAL(&scan_lock);
}

static void mark_overflow(uint32_t generation) {
    portENTER_CRITICAL(&scan_lock);
    if (active_attempt == generation) overflow_attempt = generation;
    portEXIT_CRITICAL(&scan_lock);
}
static bool report_overflowed(uint32_t generation) {
    portENTER_CRITICAL(&scan_lock);
    bool overflow = overflow_attempt == generation;
    portEXIT_CRITICAL(&scan_lock);
    return overflow;
}

static void finish(bool pass, const char *reason) {
    if (finished) return;
    finished = true; armed = false; ready_pending = false;
    memcpy(consumed_session, target.session, sizeof(consumed_session));
    if (busy) {
        /* IDF 5.3.2 does NOT send DISC_COMPLETE after application cancel. */
        int rc = ble_gap_disc_cancel();
        if (rc == 0 || rc == BLE_HS_EALREADY) {
            clear_active(attempt); busy = false;
        } else {
            pass = false; reason = "scan_cancel_failed_wait_or_reset";
            /* Remain busy until natural completion; never reconfigure a scan
               whose controller state could not be established safely. */
        }
    }
    if (!emit("result", pass ? "PASS！" : "FAIL", reason)) {
        write_line("[ble-verifier] result serialization failed; reset station");
        pass = false;
    }
    beep(pass ? 2 : 3, pass ? 120 : 180, 120);
}

static int gap_event(struct ble_gap_event *event, void *arg) {
    uint32_t generation = (uint32_t)(uintptr_t)arg;
    uint8_t expected[6];
    portENTER_CRITICAL(&scan_lock);
    bool active = active_attempt == generation && generation != 0;
    if (active) memcpy(expected, active_address, sizeof(expected));
    if (active && event->type == BLE_GAP_EVENT_DISC_COMPLETE) completed_attempt = generation;
    portEXIT_CRITICAL(&scan_lock);
    if (!active || event->type != BLE_GAP_EVENT_DISC) return 0;
    for (unsigned i = 0; i < 6; ++i) {
        if (event->disc.addr.val[5 - i] != expected[i]) return 0;
    }
    report_t report = {.attempt = generation};
    if (event->disc.length_data > sizeof(report.data)) {
        mark_overflow(generation); return 0;
    }
    report.address_type = event->disc.addr.type;
    report.length = event->disc.length_data;
    report.response = event->disc.event_type == BLE_HCI_ADV_RPT_EVTYPE_SCAN_RSP;
    memcpy(report.data, event->disc.data, report.length);
    if (xQueueSend(reports, &report, 0) != pdTRUE) mark_overflow(generation);
    return 0;
}

static void start_attempt(void) {
    if (busy) return;
    if (!armed || ready_pending || !(xEventGroupGetBits(events) & HOST_READY)) {
        write_line("[ble-verifier] not armed/ready; send BLE_TARGET before pressing");
        beep(3, 80, 80); return;
    }
    if (attempt == UINT32_MAX) {
        armed = false; write_line("[ble-verifier] attempt counter exhausted; reset station"); return;
    }
    ++attempt;
    portENTER_CRITICAL(&scan_lock);
    active_attempt = attempt; completed_attempt = overflow_attempt = 0;
    memcpy(active_address, target.address, sizeof(active_address));
    portEXIT_CRITICAL(&scan_lock);
    memset(&evidence, 0, sizeof(evidence));
    observed = type_seen = ambiguous_type = finished = false;
    ready_pending = false; busy = true;
    if (!emit("start", NULL, NULL)) {
        clear_active(attempt); busy = false; finish(false, "start_serialization_failed"); return;
    }
    const struct ble_gap_disc_params params = {
        .itvl = 160, .window = 160, .filter_policy = 0,
        .limited = 0, .passive = 0, .filter_duplicates = 0
    };
    int rc = ble_gap_disc(own_address_type, BV_SCAN_MS, &params, gap_event, (void *)(uintptr_t)attempt);
    if (rc) {
        clear_active(attempt); busy = false; finish(false, "scan_start_failed"); return;
    }
    deadline_us = esp_timer_get_time() + (int64_t)(BV_SCAN_MS + 2000) * 1000;
    beep(1, 40, 0);
}

static void process_report(const report_t *report) {
    if (!busy || report->attempt != attempt) return;
    if (report->complete) {
        clear_active(attempt); busy = false;
        if (!finished) finish(false, ambiguous_type ? "ambiguous_address_type" : bv_failure_reason(&evidence, observed));
        return;
    }
    if (report_overflowed(attempt)) { finish(false, "scan_report_overflow"); return; }
    if (finished) return;
    observed = true;
    if (!type_seen) { selected_type = report->address_type; type_seen = true; }
    else if (selected_type != report->address_type) { ambiguous_type = true; return; }
    bv_observe(&evidence, report->data, report->length, report->response);
    if (!ambiguous_type && bv_passes(&evidence)) {
        if (report_overflowed(attempt)) finish(false, "scan_report_overflow");
        else finish(true, NULL);
    }
}

static void command_line(input_line_t *input) {
    if (busy) { write_line("[ble-verifier] busy; target command ignored, wait for completion or reset"); return; }
    bv_target_t configured;
    if (input->overflow || !bv_parse_target(input->data, input->length, &configured)) {
        armed = false; ready_pending = false;
        write_line("[ble-verifier] invalid command; target disarmed"); return;
    }
    if (!strcmp(configured.session, consumed_session)) {
        armed = false; ready_pending = false;
        write_line("[ble-verifier] session already consumed; reconnect with a fresh session"); return;
    }
    if (!armed || strcmp(configured.session, target.session) || strcmp(configured.mac, target.mac)) {
        if (target_revision == UINT64_MAX) {
            armed = false; ready_pending = false;
            write_line("[ble-verifier] configuration counter exhausted; reset station"); return;
        }
        ++target_revision;
    }
    target = configured; armed = true; ready_pending = true;
    if (xEventGroupGetBits(events) & HOST_READY) {
        if (emit("ready", NULL, NULL)) ready_pending = false;
    }
}
static void input_bytes(input_line_t *input, const uint8_t *bytes, size_t count) {
    for (size_t i = 0; i < count; ++i) {
        if (bytes[i] == '\n') {
            if (input->length || input->overflow) command_line(input);
            input->length = 0; input->overflow = false;
        } else if (input->length < sizeof(input->data)) input->data[input->length++] = (char)bytes[i];
        else input->overflow = true;
    }
}

static void on_sync(void) {
    if (ble_hs_util_ensure_addr(0) == 0 && ble_hs_id_infer_auto(0, &own_address_type) == 0) xEventGroupSetBits(events, HOST_READY);
}
static void host_task(void *unused) { (void)unused; nimble_port_run(); nimble_port_freertos_deinit(); }

static void io_task(void *unused) {
    (void)unused;
    input_line_t uart_input = {0}, usb_input = {0};
    uint8_t bytes[128];
    int sample = 1, stable = 1;
    int64_t changed = esp_timer_get_time(), pressed = 0;
    uint64_t pressed_revision = 0;
    write_line("[ble-verifier] 0.1.0 ready for BLE_TARGET; active scan only, no pairing");
    while (true) {
        int n = uart_read_bytes(UART_NUM_0, bytes, sizeof(bytes), 0);
        if (n > 0) input_bytes(&uart_input, bytes, (size_t)n);
        n = usb_serial_jtag_read_bytes(bytes, sizeof(bytes), 0);
        if (n > 0) input_bytes(&usb_input, bytes, (size_t)n);
        report_t report;
        while (xQueueReceive(reports, &report, 0) == pdTRUE) process_report(&report);
        portENTER_CRITICAL(&scan_lock);
        uint32_t completed = completed_attempt;
        completed_attempt = 0;
        portEXIT_CRITICAL(&scan_lock);
        if (completed) {
            /* A final report may have arrived between the first empty-queue
               observation and the completion callback. Consume it first. */
            while (xQueueReceive(reports, &report, 0) == pdTRUE) process_report(&report);
            report_t end = {.attempt = completed, .complete = true};
            process_report(&end);
        }
        int64_t now = esp_timer_get_time();
        if (busy && !finished && (report_overflowed(attempt))) finish(false, "scan_report_overflow");
        if (busy && !finished && now >= deadline_us) finish(false, "scan_completion_timeout");
        if (armed && ready_pending && (xEventGroupGetBits(events) & HOST_READY) && emit("ready", NULL, NULL)) ready_pending = false;
        int level = gpio_get_level(BUTTON_PIN);
        if (level != sample) { sample = level; changed = now; }
        if (stable != sample && now - changed >= 30000) {
            stable = sample;
            if (!stable) {
                pressed = armed && !ready_pending && !busy && (xEventGroupGetBits(events) & HOST_READY) ? now : 0;
                pressed_revision = target_revision;
            }
            else if (pressed && pressed_revision == target_revision && now - pressed >= 40000 && now - pressed <= 1000000) { start_attempt(); pressed = 0; }
            else pressed = 0;
        }
        tick_beep(now);
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}

void app_main(void) {
    ESP_ERROR_CHECK(nvs_flash_init());
    const uart_config_t uart = {.baud_rate = 115200, .data_bits = UART_DATA_8_BITS,
        .parity = UART_PARITY_DISABLE, .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE, .source_clk = UART_SCLK_DEFAULT};
    ESP_ERROR_CHECK(uart_driver_install(UART_NUM_0, 2048, 2048, 0, NULL, 0));
    ESP_ERROR_CHECK(uart_param_config(UART_NUM_0, &uart));
    ESP_ERROR_CHECK(uart_set_pin(UART_NUM_0, 43, 44, UART_PIN_NO_CHANGE, UART_PIN_NO_CHANGE));
    const usb_serial_jtag_driver_config_t usb = {.tx_buffer_size = 2048, .rx_buffer_size = 2048};
    ESP_ERROR_CHECK(usb_serial_jtag_driver_install(&usb));
    const gpio_config_t button = {.pin_bit_mask = 1ULL << BUTTON_PIN, .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE, .pull_down_en = GPIO_PULLDOWN_DISABLE, .intr_type = GPIO_INTR_DISABLE};
    ESP_ERROR_CHECK(gpio_config(&button));
    const ledc_timer_config_t timer = {.speed_mode = LEDC_LOW_SPEED_MODE, .duty_resolution = LEDC_TIMER_8_BIT,
        .timer_num = LEDC_TIMER_0, .freq_hz = 4000, .clk_cfg = LEDC_AUTO_CLK};
    ESP_ERROR_CHECK(ledc_timer_config(&timer));
    const ledc_channel_config_t buzzer = {.gpio_num = BUZZER_PIN, .speed_mode = LEDC_LOW_SPEED_MODE,
        .channel = LEDC_CHANNEL_0, .intr_type = LEDC_INTR_DISABLE, .timer_sel = LEDC_TIMER_0, .duty = 0, .hpoint = 0};
    ESP_ERROR_CHECK(ledc_channel_config(&buzzer));
    reports = xQueueCreate(REPORT_QUEUE_LENGTH, sizeof(report_t));
    events = xEventGroupCreate();
    if (!reports || !events) abort();
    ESP_ERROR_CHECK(nimble_port_init());
    ble_hs_cfg.sync_cb = on_sync;
    nimble_port_freertos_init(host_task);
    if (xTaskCreate(io_task, "ble_verifier_io", 6144, NULL, 5, NULL) != pdPASS) abort();
}
