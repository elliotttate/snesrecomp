#include <stdio.h>
#include <string.h>

#include "apu.h"
#include "dsp_shadow.h"

static int applied_count;
static uint8_t applied_port;
static uint8_t applied_value;
uint64_t g_apu_timer0_total_ticks;
int snes_frame_counter;

void audio_trace_on_cpu_port_apply(uint8_t port, uint8_t value) {
  applied_count++;
  applied_port = port;
  applied_value = value;
}

/* Standalone-link stubs for core paths which the zero-cycle handshake checks
 * do not execute. Keeping these here lets the checked-in runner compile and
 * execute this test instead of leaving it as an unregistered source file. */
void audio_trace_on_spc_port_read(uint8_t port, uint8_t value) {
  (void)port; (void)value;
}
void audio_trace_on_spc_port_write(uint8_t port, uint8_t value) {
  (void)port; (void)value;
}
void audio_trace_on_sample(int16_t left, int16_t right, int dropped,
                           uint32_t ring_fill) {
  (void)left; (void)right; (void)dropped; (void)ring_fill;
}
void audio_trace_on_reg_write(uint8_t address, uint8_t value) {
  (void)address; (void)value;
}
void audio_trace_on_consume(uint64_t read_index, uint32_t count,
                            uint32_t available_after) {
  (void)read_index; (void)count; (void)available_after;
}
void audio_trace_sample_clocks(uint64_t *produced, uint64_t *consumed) {
  if (produced) *produced = 0;
  if (consumed) *consumed = 0;
}
DspShadow *dsp_shadow_create(void) { return NULL; }
void dsp_shadow_free(DspShadow *shadow) { (void)shadow; }
void dsp_shadow_process(DspShadow *shadow, Dsp *dsp, int canonical_left,
                        int canonical_right, int *out_left, int *out_right) {
  (void)shadow; (void)dsp;
  *out_left = canonical_left;
  *out_right = canonical_right;
}

static int check(int condition, const char *message) {
  if (!condition)
    fprintf(stderr, "FAIL: %s\n", message);
  return condition ? 0 : 1;
}

int main(void) {
  Apu apu;
  int failures = 0;

  memset(&apu, 0, sizeof(apu));

  apu_writePortNow(&apu, 2, 0x80);
  failures += check(apu.inPorts[2] == 0x80,
                    "CPU port write is visible in the current APU cycle");
  failures += check(applied_count == 1 && applied_port == 2 &&
                    applied_value == 0x80,
                    "CPU port write is traced when it becomes visible");

  apu_writePortNow(&apu, 2, 0x23);
  failures += check(apu.inPorts[2] == 0x23,
                    "a later command replaces the port immediately");

  apu_writePortNow(&apu, 6, 0x00);
  failures += check(apu.inPorts[2] == 0x00 && applied_port == 2 &&
                    applied_value == 0x00,
                    "port index is masked and clear is immediately visible");

  memset(&apu, 0, sizeof(apu));
  applied_count = 0;
  apu_schedulePortWrite(&apu, 2, 0x80, 100);
  apu_schedulePortWrite(&apu, 2, 0x23, 200);
  failures += check(apu.inPorts[2] == 0 && applied_count == 0,
                    "deferred commands remain hidden until APU time advances");
  apu_flushPortQueueNow(&apu);
  failures += check(apu.inPorts[2] == 0x23 && applied_count == 2 &&
                    apu.portQHead == apu.portQTail,
                    "HLE takeover preserves queued ordering and final bus state");

  apu_schedulePortWrite(&apu, 2, 0x45, 300);
  apu_clearPortQueue(&apu);
  apu_flushPortQueueNow(&apu);
  failures += check(apu.inPorts[2] == 0x23 && applied_count == 2,
                    "queue clear discards stale post-reset commands");

  memset(&apu, 0, sizeof(apu));
  apu.outPorts[0] = 0xaa;
  apu.outPorts[1] = 0xbb;
  failures += check(apu_waitForTransferReady(&apu, 1, 0xff, 0),
                    "HLE upload waits for the SPC transfer-ready handshake");

  apu.outPorts[0] = 0;
  failures += check(!apu_waitForTransferReady(&apu, 1, 0xff, 0),
                    "HLE upload rejects a missing transfer-ready handshake");
  failures += check(apu.inPorts[1] == 0xff,
                    "HLE upload reasserts a transfer request cleared during startup");

  apu.outPorts[0] = 0xcc;
  failures += check(apu_finishHleTransfer(&apu, 0x1234, 0),
                    "HLE upload waits for the SPC terminator acknowledgement");
  failures += check(apu.inPorts[0] == 0 && apu.inPorts[1] == 0 &&
                    apu.inPorts[2] == 0 && apu.inPorts[3] == 0,
                    "HLE upload clears CPU ports after acknowledgement");

  if (failures)
    return 1;
  puts("apu_port_transition_test: PASS");
  return 0;
}
