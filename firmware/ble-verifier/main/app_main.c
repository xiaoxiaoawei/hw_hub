#include "ble_protocol.h"
#include "station_feedback.h"
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "cJSON.h"
#include "driver/gpio.h"
#include "driver/ledc.h"
#include "driver/uart.h"
#include "driver/usb_serial_jtag.h"
#include "esp_app_desc.h"
#include "esp_err.h"
#include "esp_random.h"
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
#define HARDWARE_READY BIT1
#define INITIALIZED BIT2
#define BV_SETTLE_US 2000000LL
#define REPORT_QUEUE_LENGTH 64

typedef struct {
    uint32_t attempt;
    uint8_t address[6], address_type, length;
    bool response;
    uint8_t data[BV_MAX_AD_BYTES];
} report_t;
typedef enum { BOOT_SERIAL, BOOT_NVS, BOOT_BLE, BOOT_READY, BOOT_FAILED } boot_phase_t;

static QueueHandle_t reports;
static QueueHandle_t sound_commands;
static EventGroupHandle_t events;
static portMUX_TYPE state_lock = portMUX_INITIALIZER_UNLOCKED;
static uint32_t active_attempt, completed_attempt, overflow_attempt, callback_inflight;
typedef struct { uint8_t address[6], type; } known_key_t;
static known_key_t known_keys[BV_MAX_CANDIDATES];
static size_t known_count;
static bool uart_ready, usb_ready, sound_busy;
static unsigned sound_pending_commands;
static boot_phase_t boot_phase = BOOT_SERIAL;
static esp_err_t boot_error;
static const char *boot_failure = "none";
static char session[65];
static uint8_t own_address_type;
/* Scan/evidence state belongs only to the I/O task. Callbacks only copy
   bounded packets and update generation-tagged status under state_lock. */
static bool busy, finished, finishing;
static uint32_t attempt;
static bv_scan_t scan;
static int64_t radio_end_us, first_valid_us;

static void phase(boot_phase_t value, const char *reason, esp_err_t error) {
    portENTER_CRITICAL(&state_lock);
    boot_phase = value; boot_failure = reason; boot_error = error;
    portEXIT_CRITICAL(&state_lock);
}
static bool healthy(void) {
    portENTER_CRITICAL(&state_lock);
    bool failed = boot_phase == BOOT_FAILED;
    portEXIT_CRITICAL(&state_lock);
    const EventBits_t required = HOST_READY | HARDWARE_READY | INITIALIZED;
    return !failed && events && (xEventGroupGetBits(events) & required) == required;
}
static bool sound_is_busy(void) {
    portENTER_CRITICAL(&state_lock); bool value=sound_busy; portEXIT_CRITICAL(&state_lock);
    return value;
}
static bool request_sound(bv_sound_kind_t kind) {
    if (!sound_commands || !events || !(xEventGroupGetBits(events)&HARDWARE_READY)) return false;
    /* Publish busy before enqueueing so a rapid second button release cannot
       overwrite a sound which its independent task has not received yet. */
    portENTER_CRITICAL(&state_lock); ++sound_pending_commands; sound_busy=true; portEXIT_CRITICAL(&state_lock);
    if (xQueueSend(sound_commands,&kind,0)==pdTRUE) return true;
    portENTER_CRITICAL(&state_lock); --sound_pending_commands; portEXIT_CRITICAL(&state_lock);
    phase(BOOT_FAILED,"buzzer_queue_failed",ESP_FAIL); return false;
}
static void sound_task(void *unused) {
    (void)unused;
    bv_sound_state_t state={0};
    uint32_t previous_hz=UINT32_MAX;
    while (true) {
        bv_sound_kind_t kind;
        while (xQueueReceive(sound_commands,&kind,0)==pdTRUE) {
            uint64_t now_ms=(uint64_t)esp_timer_get_time()/1000;
            portENTER_CRITICAL(&state_lock); --sound_pending_commands; portEXIT_CRITICAL(&state_lock);
            if (kind==BV_SOUND_START) bv_sound_start(&state,now_ms);
            else bv_sound_result(&state,kind,now_ms);
        }
        uint64_t now_ms=(uint64_t)esp_timer_get_time()/1000;
        uint32_t hz=bv_sound_tick(&state,now_ms);
        if (hz!=previous_hz) {
            esp_err_t rc=ESP_OK;
            if (hz) rc=ledc_set_freq(LEDC_LOW_SPEED_MODE,LEDC_TIMER_0,hz);
            if (rc==ESP_OK) rc=ledc_set_duty(LEDC_LOW_SPEED_MODE,LEDC_CHANNEL_0,hz?128:0);
            if (rc==ESP_OK) rc=ledc_update_duty(LEDC_LOW_SPEED_MODE,LEDC_CHANNEL_0);
            if (rc!=ESP_OK) {
                ledc_set_duty(LEDC_LOW_SPEED_MODE,LEDC_CHANNEL_0,0);
                ledc_update_duty(LEDC_LOW_SPEED_MODE,LEDC_CHANNEL_0);
                phase(BOOT_FAILED,"buzzer_output_failed",rc);
            }
            previous_hz=hz;
        }
        portENTER_CRITICAL(&state_lock);
        sound_busy=state.active!=BV_SOUND_NONE || sound_pending_commands!=0;
        portEXIT_CRITICAL(&state_lock);
        /* Audio never blocks UART/BLE processing or the start of scanning.
           128/256 is the maximum useful square-wave duty for a passive buzzer;
           GPIO voltage and drive strength are unchanged. */
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}
static void write_line(const char *text) {
    portENTER_CRITICAL(&state_lock);
    bool uart = uart_ready, usb = usb_ready;
    portEXIT_CRITICAL(&state_lock);
    /* Primary onboard USB-to-TTL bridge is independent of native USB. */
    if (uart) { uart_write_bytes(UART_NUM_0, text, strlen(text)); uart_write_bytes(UART_NUM_0, "\n", 1); }
    /* The onboard UART bridge is primary. An installed but physically unused
       native USB driver must not spend 2x20ms waiting on each log line. */
    if (usb && usb_serial_jtag_is_connected()) {
        usb_serial_jtag_write_bytes(text, strlen(text), pdMS_TO_TICKS(20));
        usb_serial_jtag_write_bytes("\n", 1, pdMS_TO_TICKS(20));
    }
}
static void hex_bytes(const uint8_t *data, size_t size, char *out) {
    static const char alphabet[] = "0123456789ABCDEF";
    for (size_t i = 0; i < size; ++i) { out[2*i] = alphabet[data[i] >> 4]; out[2*i+1] = alphabet[data[i] & 15]; }
    out[2*size] = 0;
}
static bool emit(const char *kind, const char *status, const char *reason, const bv_candidate_t *candidate) {
    cJSON *json = cJSON_CreateObject();
    if (!json) return false;
    bool valid = cJSON_AddStringToObject(json, "event", kind) != NULL;
    valid &= cJSON_AddStringToObject(json, "session", session) != NULL;
    if (!strcmp(kind, "ready")) valid &= cJSON_AddNumberToObject(json, "protocol", 2) != NULL;
    else valid &= cJSON_AddNumberToObject(json, "attempt", attempt) != NULL;
    if (!strcmp(kind,"ready") || !strcmp(kind,"start"))
        valid &= cJSON_AddNumberToObject(json,"scan_timeout_ms",BV_SCAN_MS) != NULL;
    if (status) valid &= cJSON_AddStringToObject(json, "status", status) != NULL;
    if (reason) valid &= cJSON_AddStringToObject(json, "reason", reason) != NULL;
    if (candidate) {
        char mac[13], raw[2 * BV_MAX_AD_BYTES + 1];
        hex_bytes(candidate->address, 6, mac);
        valid &= cJSON_AddStringToObject(json, "mac", mac) != NULL;
        if (candidate->evidence.name_seen) valid &= cJSON_AddStringToObject(json, "name", candidate->evidence.name) != NULL;
        if (candidate->evidence.diagnostic_seen) {
            char version[16];
            snprintf(version, sizeof(version), "%u.%u", candidate->evidence.major, candidate->evidence.minor);
            valid &= cJSON_AddStringToObject(json, "firmware", version) != NULL;
            valid &= cJSON_AddNumberToObject(json, "fault_bits", candidate->evidence.fault_bits) != NULL;
            valid &= cJSON_AddNumberToObject(json, "battery_mv", candidate->evidence.battery_mv) != NULL;
        }
        if (candidate->adv_len) { hex_bytes(candidate->adv, candidate->adv_len, raw); valid &= cJSON_AddStringToObject(json, "adv_hex", raw) != NULL; }
        if (candidate->scan_rsp_len) { hex_bytes(candidate->scan_rsp, candidate->scan_rsp_len, raw); valid &= cJSON_AddStringToObject(json, "scan_rsp_hex", raw) != NULL; }
        /* No UID exists in the documented advertising format. Omit it. */
    }
    if (!valid) { cJSON_Delete(json); return false; }
    char *encoded = cJSON_PrintUnformatted(json); cJSON_Delete(json);
    if (!encoded) return false;
    char line[768]; int length = snprintf(line, sizeof(line), "BLE_VERIFY %s", encoded); free(encoded);
    if (length < 0 || (size_t)length >= sizeof(line)) return false;
    write_line(line); return true;
}
static void clear_active(uint32_t generation) {
    portENTER_CRITICAL(&state_lock);
    if (active_attempt == generation) active_attempt = 0;
    if (completed_attempt == generation) completed_attempt = 0;
    if (overflow_attempt == generation) overflow_attempt = 0;
    portEXIT_CRITICAL(&state_lock);
}
static bool report_overflowed(uint32_t generation) {
    portENTER_CRITICAL(&state_lock); bool result = overflow_attempt == generation; portEXIT_CRITICAL(&state_lock); return result;
}
static void process_report(const report_t *report);
static const char *scan_reason(void);
static bool settled_pass(int64_t now) {
    const bv_candidate_t *candidate=bv_scan_single(&scan);
    return healthy() && !scan.overflow && candidate && bv_candidate_passes(candidate) && first_valid_us &&
        first_valid_us+BV_SETTLE_US<=radio_end_us && now-first_valid_us>=BV_SETTLE_US;
}
static void finish(bool pass, const char *reason, bool expired) {
    if (finished || finishing) return;
    finishing=true;
    bool stopped=!busy;
    if (busy) {
        /* IDF 5.3.2 does not issue DISC_COMPLETE after successful cancel. */
        int rc=ble_gap_disc_cancel();
        stopped=rc==0 || rc==BLE_HS_EALREADY;
        if (!stopped) { pass=false; reason="scan_cancel_failed_wait_or_reset"; }
    }
    if (stopped) {
        /* Cancel prevents new HCI discovery callbacks. Let already-entered,
           nonblocking callbacks publish their final packet before closing the
           generation, then consume all those packets before deciding PASS. */
        int64_t wait_until=esp_timer_get_time()+100000;
        bool drained=false;
        while (!drained && esp_timer_get_time()<wait_until) {
            portENTER_CRITICAL(&state_lock);
            drained=callback_inflight==0;
            if (drained && active_attempt==attempt) active_attempt=0;
            portEXIT_CRITICAL(&state_lock);
            if (!drained) vTaskDelay(pdMS_TO_TICKS(1));
        }
        if (!drained) { pass=false; reason="scan_callback_drain_timeout"; }
        report_t tail;
        while (xQueueReceive(reports,&tail,0)==pdTRUE) process_report(&tail);
        bool overflow=report_overflowed(attempt);
        if (overflow) { pass=false; reason="scan_report_overflow"; }
        else if (pass && !settled_pass(esp_timer_get_time())) {
            pass=false; reason=scan_reason();
            /* Finishing a settled observation may itself cross the deadline.
               Preserve timeout status if final tail packets invalidate PASS. */
            expired=expired || esp_timer_get_time()>=radio_end_us;
        } else if (!pass && expired && drained) reason=scan_reason();
        clear_active(attempt); busy=false;
    }
    finished=true; finishing=false;
    const bv_candidate_t *candidate=bv_scan_single(&scan);
    char timeout_reason[128];
    if (!pass && expired) reason=bv_timeout_reason(reason,timeout_reason,sizeof(timeout_reason));
    if (!emit("result",pass?"PASS！":"FAIL",pass?NULL:reason,candidate)) {
        write_line("[ble-verifier] result serialization failed"); pass=false;
    }
    request_sound(pass?BV_SOUND_PASS:BV_SOUND_FAIL);
}

static const char *scan_reason(void) {
    if (scan.overflow) return "candidate_capacity_exceeded";
    size_t count = bv_scan_matching_count(&scan);
    if (count > 1) return "multiple_lumysi_candidates";
    const bv_candidate_t *candidate = bv_scan_single(&scan);
    if (!candidate) return "no_lumysi_candidate";
    if (bv_candidate_passes(candidate)) return "settle_window_incomplete";
    return bv_failure_reason(&candidate->evidence, true);
}
static int gap_event(struct ble_gap_event *event, void *arg) {
    uint32_t generation=(uint32_t)(uintptr_t)arg;
    portENTER_CRITICAL(&state_lock);
    bool active=active_attempt==generation && generation!=0;
    if (active && event->type==BLE_GAP_EVENT_DISC_COMPLETE) completed_attempt=generation;
    bool known=false;
    if (active && event->type==BLE_GAP_EVENT_DISC) {
        ++callback_inflight;
        for (size_t i=0;i<known_count && !known;++i) {
            if (known_keys[i].type!=event->disc.addr.type) continue;
            known=true;
            for (unsigned n=0;n<6;++n) if (known_keys[i].address[n]!=event->disc.addr.val[5-n]) known=false;
        }
    }
    portEXIT_CRITICAL(&state_lock);
    if (!active || event->type!=BLE_GAP_EVENT_DISC) return 0;
    report_t report={.attempt=generation};
    bool overflow=event->disc.length_data>sizeof(report.data);
    if (!overflow) {
        for (unsigned i=0;i<6;++i) report.address[i]=event->disc.addr.val[5-i];
        report.address_type=event->disc.addr.type; report.length=event->disc.length_data;
        report.response=event->disc.event_type==BLE_HCI_ADV_RPT_EVTYPE_SCAN_RSP;
        if (known || bv_packet_interesting(event->disc.data,report.length,report.response)) {
            memcpy(report.data,event->disc.data,report.length);
            portENTER_CRITICAL(&state_lock);
            bool publish=active_attempt==generation;
            if (publish && !known) {
                bool exists=false;
                for (size_t i=0;i<known_count;++i) {
                    if (known_keys[i].type==report.address_type && !memcmp(known_keys[i].address,report.address,6)) exists=true;
                }
                if (!exists) {
                    if (known_count==BV_MAX_CANDIDATES) { publish=false; overflow=true; }
                    else {
                        known_keys[known_count].type=report.address_type;
                        memcpy(known_keys[known_count++].address,report.address,6);
                    }
                }
            }
            portEXIT_CRITICAL(&state_lock);
            /* Register a plausible address before returning from its first
               callback, so an immediate following RSP cannot be filtered out
               while the I/O task has not consumed the first ADV yet. */
            if (publish && xQueueSend(reports,&report,0)!=pdTRUE) overflow=true;
        }
    }
    portENTER_CRITICAL(&state_lock);
    if (overflow && active_attempt==generation) overflow_attempt=generation;
    --callback_inflight;
    portEXIT_CRITICAL(&state_lock);
    return 0;
}

static void start_attempt(void) {
    if (busy || sound_is_busy()) return;
    if (!healthy()) { write_line("[ble-verifier] initialization failed/not ready; no scan started"); return; }
    if (attempt == UINT32_MAX) { phase(BOOT_FAILED, "attempt_counter_exhausted", ESP_FAIL); return; }
    ++attempt; bv_scan_reset(&scan); finished = false; first_valid_us = 0; busy = true;
    portENTER_CRITICAL(&state_lock); active_attempt = attempt; completed_attempt = overflow_attempt = 0; known_count=0; portEXIT_CRITICAL(&state_lock);
    if (!request_sound(BV_SOUND_START)) { clear_active(attempt); busy=false; finish(false,"buzzer_start_failed",false); return; }
    if (!emit("start", NULL, NULL, NULL)) { clear_active(attempt); busy = false; finish(false, "start_serialization_failed",false); return; }
    const struct ble_gap_disc_params params = {.itvl=160, .window=160, .filter_policy=0, .limited=0, .passive=0, .filter_duplicates=0};
    radio_end_us = esp_timer_get_time() + (int64_t)BV_SCAN_MS * 1000;
    int rc = ble_gap_disc(own_address_type, BV_SCAN_MS, &params, gap_event, (void *)(uintptr_t)attempt);
    if (rc) { clear_active(attempt); busy = false; finish(false,"scan_start_failed",false); return; }
}
static void process_report(const report_t *report) {
    if (!busy || finished || report->attempt != attempt) return;
    if (!finishing && report_overflowed(attempt)) { finish(false,"scan_report_overflow",false); return; }
    bv_scan_observe(&scan, report->address, report->address_type, report->data, report->length, report->response);
    if (scan.overflow && !finishing) { finish(false,"candidate_capacity_exceeded",false); return; }
    if (finishing) return;
    if (bv_scan_matching_count(&scan) > 1) { finish(false,"multiple_lumysi_candidates",false); return; }
    const bv_candidate_t *candidate = bv_scan_single(&scan);
    if (!first_valid_us && candidate && bv_candidate_passes(candidate)) first_valid_us = esp_timer_get_time();
}
static void on_sync(void) {
    if (ble_hs_util_ensure_addr(0) == 0 && ble_hs_id_infer_auto(0, &own_address_type) == 0) {
        portENTER_CRITICAL(&state_lock);
        if (boot_phase!=BOOT_FAILED) { boot_phase=BOOT_READY; boot_failure="ready"; boot_error=ESP_OK; }
        portEXIT_CRITICAL(&state_lock);
        xEventGroupSetBits(events, HOST_READY);
    } else phase(BOOT_FAILED, "ble_address_init_failed", ESP_FAIL);
}
static void on_reset(int reason) { xEventGroupClearBits(events, HOST_READY); phase(BOOT_FAILED,"ble_host_reset",reason); }
static void host_task(void *unused) { (void)unused; nimble_port_run(); nimble_port_freertos_deinit(); }
static void heartbeat(void) {
    portENTER_CRITICAL(&state_lock);
    boot_phase_t current = boot_phase; esp_err_t error = boot_error; const char *failure = boot_failure;
    bool uart = uart_ready, usb = usb_ready;
    portEXIT_CRITICAL(&state_lock);
    char line[192];
    snprintf(line,sizeof(line),"[ble-verifier] firmware=%s protocol=2 phase=%u error=0x%X reason=%s uart0=%s usb_jtag=%s busy=%u",
             esp_app_get_description()->version,(unsigned)current,(unsigned)error,failure,uart?"up":"down",usb?"up":"optional-unavailable",busy?1:0);
    write_line(line);
    if (healthy()) emit("ready",NULL,NULL,NULL);
}
static void io_task(void *unused) {
    (void)unused;
    int sample=1, stable=1; int64_t changed=esp_timer_get_time(), pressed=0, next_heartbeat=0;
    while (true) {
        int64_t now=esp_timer_get_time();
        uint8_t ignored[128]; /* No command or application write is required. */
        if (uart_ready) uart_read_bytes(UART_NUM_0,ignored,sizeof(ignored),0);
        portENTER_CRITICAL(&state_lock); bool usb=usb_ready; portEXIT_CRITICAL(&state_lock);
        if (usb) usb_serial_jtag_read_bytes(ignored,sizeof(ignored),0);
        report_t report;
        for (unsigned n=0;n<256 && xQueueReceive(reports,&report,0)==pdTRUE;++n) process_report(&report);
        portENTER_CRITICAL(&state_lock); uint32_t completed=completed_attempt; completed_attempt=0; portEXIT_CRITICAL(&state_lock);
        if (completed && busy && completed==attempt) {
            /* Preserve the last queued ADV/RSP before handling completion. */
            while (xQueueReceive(reports,&report,0)==pdTRUE) process_report(&report);
            if (busy && !finished) {
                now=esp_timer_get_time();
                /* NimBLE's OS tick timer can expire just before our microsecond
                   deadline. One scheduler tick is rounding, not an early abort. */
                bool expired=now>=radio_end_us-(int64_t)portTICK_PERIOD_MS*1000;
                finish(settled_pass(now),expired?scan_reason():"scan_ended_early",expired);
            }
            else { clear_active(attempt); busy=false; }
        }
        now=esp_timer_get_time();
        if (busy && !finished) {
            if (report_overflowed(attempt)) finish(false,"scan_report_overflow",false);
            else if (!healthy()) finish(false,"station_not_ready",false);
            else if (first_valid_us && first_valid_us+BV_SETTLE_US<=radio_end_us && now-first_valid_us>=BV_SETTLE_US) {
                while (xQueueReceive(reports,&report,0)==pdTRUE) process_report(&report);
                const bv_candidate_t *candidate=bv_scan_single(&scan);
                bool pass=candidate && bv_candidate_passes(candidate) && !scan.overflow && !report_overflowed(attempt);
                if (busy && !finished) finish(pass,pass?NULL:scan_reason(),esp_timer_get_time()>=radio_end_us);
            } else if (now>=radio_end_us) finish(false,scan_reason(),true);
        }
        if (events && (xEventGroupGetBits(events)&HARDWARE_READY)) {
            int level=gpio_get_level(BUTTON_PIN);
            if (level!=sample) { sample=level; changed=now; }
            if (stable!=sample && now-changed>=30000) {
                stable=sample;
                if (!stable) pressed=healthy()&&!busy&&!sound_is_busy()?now:0;
                else { if (pressed && now-pressed>=40000 && now-pressed<=1000000) start_attempt(); pressed=0; }
            }
        }
        /* BLE report consumption, button handling and the hard scan deadline
           take precedence over optional periodic serial diagnostics. */
        now=esp_timer_get_time();
        if (now>=next_heartbeat) { heartbeat(); next_heartbeat=now+1000000; }
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}

void app_main(void) {
    /* UART0 comes first. Never hide an NVS/BLE startup failure behind console
       NONE, or require a host write before a late-opened monitor sees output. */
    const uart_config_t uart={.baud_rate=115200,.data_bits=UART_DATA_8_BITS,.parity=UART_PARITY_DISABLE,
        .stop_bits=UART_STOP_BITS_1,.flow_ctrl=UART_HW_FLOWCTRL_DISABLE,.source_clk=UART_SCLK_DEFAULT};
    esp_err_t rc=uart_driver_install(UART_NUM_0,2048,2048,0,NULL,0);
    if (rc==ESP_OK) rc=uart_param_config(UART_NUM_0,&uart);
    if (rc==ESP_OK) rc=uart_set_pin(UART_NUM_0,43,44,UART_PIN_NO_CHANGE,UART_PIN_NO_CHANGE);
    uart_ready=rc==ESP_OK;
    char boot_line[160];
    snprintf(boot_line,sizeof(boot_line),"[ble-verifier] BOOT %s UART0 primary 115200 8N1; no host command required",esp_app_get_description()->version);
    write_line(boot_line);
    reports=xQueueCreate(REPORT_QUEUE_LENGTH,sizeof(report_t)); events=xEventGroupCreate();
    if (!reports || !events) { while (true) { write_line("[ble-verifier] FATAL queue allocation failed"); vTaskDelay(pdMS_TO_TICKS(1000)); } }
    uint8_t nonce[32]; esp_fill_random(nonce,sizeof(nonce));
    static const char lower[]="0123456789abcdef";
    for (unsigned i=0;i<32;++i) { session[2*i]=lower[nonce[i]>>4]; session[2*i+1]=lower[nonce[i]&15]; }
    session[64]=0;
    const gpio_config_t button={.pin_bit_mask=1ULL<<BUTTON_PIN,.mode=GPIO_MODE_INPUT,.pull_up_en=GPIO_PULLUP_ENABLE,
        .pull_down_en=GPIO_PULLDOWN_DISABLE,.intr_type=GPIO_INTR_DISABLE};
    esp_err_t hardware=gpio_config(&button);
    const ledc_timer_config_t timer={.speed_mode=LEDC_LOW_SPEED_MODE,.duty_resolution=LEDC_TIMER_8_BIT,.timer_num=LEDC_TIMER_0,.freq_hz=4000,.clk_cfg=LEDC_AUTO_CLK};
    if (hardware==ESP_OK) hardware=ledc_timer_config(&timer);
    const ledc_channel_config_t buzzer={.gpio_num=BUZZER_PIN,.speed_mode=LEDC_LOW_SPEED_MODE,.channel=LEDC_CHANNEL_0,
        .intr_type=LEDC_INTR_DISABLE,.timer_sel=LEDC_TIMER_0,.duty=0,.hpoint=0};
    if (hardware==ESP_OK) hardware=ledc_channel_config(&buzzer);
    if (hardware==ESP_OK) {
        sound_commands=xQueueCreate(4,sizeof(bv_sound_kind_t));
        if (!sound_commands || xTaskCreate(sound_task,"ble_verifier_sound",3072,NULL,6,NULL)!=pdPASS) hardware=ESP_ERR_NO_MEM;
    }
    if (hardware==ESP_OK) xEventGroupSetBits(events,HARDWARE_READY);
    if (xTaskCreate(io_task,"ble_verifier_io",8192,NULL,5,NULL)!=pdPASS) { while (true) { write_line("[ble-verifier] FATAL I/O task allocation failed"); vTaskDelay(pdMS_TO_TICKS(1000)); } }
    /* Native USB is optional and its success never gates the onboard bridge. */
    usb_serial_jtag_driver_config_t usb={.tx_buffer_size=2048,.rx_buffer_size=2048};
    esp_err_t usb_rc=usb_serial_jtag_driver_install(&usb);
    portENTER_CRITICAL(&state_lock); usb_ready=usb_rc==ESP_OK; portEXIT_CRITICAL(&state_lock);
    if (hardware!=ESP_OK) { phase(BOOT_FAILED,"button_or_buzzer_init_failed",hardware); return; }
    if (!uart_ready && !usb_ready) { phase(BOOT_FAILED,"no_serial_transport",rc); return; }
    phase(BOOT_NVS,"nvs_initializing",ESP_OK);
    rc=nvs_flash_init();
    if (rc!=ESP_OK) { phase(BOOT_FAILED,"nvs_init_failed",rc); return; }
    phase(BOOT_BLE,"ble_initializing",ESP_OK);
    rc=nimble_port_init();
    if (rc!=ESP_OK) { phase(BOOT_FAILED,"ble_init_failed",rc); return; }
    ble_hs_cfg.sync_cb=on_sync; ble_hs_cfg.reset_cb=on_reset;
    xEventGroupSetBits(events,INITIALIZED);
    phase(BOOT_READY,"waiting_for_radio_sync",ESP_OK);
    nimble_port_freertos_init(host_task);
}
