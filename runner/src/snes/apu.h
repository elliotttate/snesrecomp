
#ifndef APU_H
#define APU_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdbool.h>

typedef struct Apu Apu;

#include "snes.h"
#include "spc.h"
#include "dsp.h"

typedef struct Timer {
  uint8_t cycles;
  uint8_t divider;
  uint8_t target;
  uint8_t counter;
  bool enabled;
} Timer;

/* Optional CPU->APU port scheduling used by games whose one-frame command
 * pulses can otherwise be overwritten between callback-driven SPC polls.
 * The default hardware path remains immediate; games opt into this scheduler
 * with SNESRECOMP_APU_IMMEDIATE_PORTS=0. */
#define APU_PORT_QUEUE_LEN 128u
#define APU_PORT_MIN_DWELL 128u

typedef struct ApuPortWrite {
  uint64_t target_sample;
  uint8_t port;
  uint8_t val;
} ApuPortWrite;

struct Apu {
  Spc* spc;
  Dsp* dsp;
  uint8_t ram[0x10000];
  bool romReadable;
  uint8_t dspAdr;
  uint32_t cycles;
  uint8_t inPorts[6]; // includes 2 bytes of ram
  uint8_t outPorts[4];
  Timer timer[3];
  uint8_t cpuCyclesLeft;
  uint8_t pad[6];
  /* Kept after the frozen savestate range ending at pad+6. Pending host-time
   * writes are intentionally discarded on reset, load, and HLE upload. */
  ApuPortWrite portQueue[APU_PORT_QUEUE_LEN];
  uint32_t portQHead;
  uint32_t portQTail;
};

Apu* apu_init();
void apu_free(Apu* apu);
void apu_reset(Apu* apu);
void apu_cycle(Apu* apu);
uint8_t apu_cpuRead(Apu* apu, uint16_t adr);
void apu_cpuWrite(Apu* apu, uint16_t adr, uint8_t val);
void apu_saveload(Apu *apu, SaveLoadInfo *sli);
/* Apply a hardware-visible CPU port write at the current SPC cycle. */
void apu_writePortNow(Apu* apu, uint8_t port, uint8_t val);
/* Schedule a write in produced-sample time. Caller holds RtlApuLock. */
void apu_schedulePortWrite(Apu* apu, uint8_t port, uint8_t val,
                           uint64_t target_sample);
void apu_clearPortQueue(Apu* apu);
/* Apply and discard all pending writes without advancing the SPC. Used when
 * the HLE upload path takes ownership of the port protocol. */
void apu_flushPortQueueNow(Apu* apu);
/* Advance until the SPC acknowledges the standard upload protocol. */
bool apu_waitForTransferReady(Apu* apu, uint8_t request_port,
                              uint8_t request_value, uint32_t max_cycles);
bool apu_finishHleTransfer(Apu* apu, uint16_t final_pc,
                           uint32_t max_cycles);
#endif
