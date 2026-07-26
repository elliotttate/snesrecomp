/* Synthetic regression for priority-band host-overlay extraction
 * (PpuBindOverlayPrioSurface) and the lazy overlay clear — the machinery the
 * layered-parallax presenter is built on (runner/src/parallax.c,
 * docs/PARALLAX.md).
 *
 * No game ROM, generated data, or platform frontend is required: the tilemap,
 * tile bitmaps, palette and OAM are all written by hand below.
 *
 * What is actually at risk here, and why each case exists:
 *   * Band ROUTING for BGs is a per-source z-rank THRESHOLD table. Get a
 *     threshold wrong and a layer's priority-1 tiles silently land in the
 *     priority-0 plane, which on screen looks like "the foreground tiles are
 *     at the wrong depth" — plausible enough to be mistaken for bad tuning.
 *   * Band routing for OBJ reads the OAM priority out of the z word's top two
 *     bits, a different encoding from the BG path.
 *   * The lazy clear is a performance optimization that can go WRONG in one
 *     direction (stale content left in a surface after the capture stops) and
 *     merely slow in the other. The stale case would show a frozen ghost layer.
 */
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "snes/ppu.h"
#include "snes/snes.h"

bool g_new_ppu = true;
Snes *g_snes;

void PpuDrawWholeLineOldPpu(Ppu *ppu, int line) {
    (void)ppu;
    (void)line;
}

uint16_t WsShadowTile(int layer, int screen_x, uint32_t wrapped_y,
                      uint16_t real_tile) {
    (void)layer;
    (void)screen_x;
    (void)wrapped_y;
    return real_tile;
}

bool WsShadowLayerActive(int layer) {
    (void)layer;
    return false;
}

uint32_t WsShadowWorldX(int layer) {
    (void)layer;
    return 0;
}

uint32_t WsShadowPresentWorldY(int layer, int screen_x) {
    (void)layer;
    (void)screen_x;
    return 0;
}

uint32_t WsShadowScrollY(int layer) {
    (void)layer;
    return 0;
}

void WsShadowOnVramWrite(uint16_t word_adr, uint16_t value) {
    (void)word_adr;
    (void)value;
}

static int g_failures;

static void check(bool condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "FAIL: %s\n", message);
        g_failures++;
    }
}

enum { kPitch = kPpuXPixels * 4, kLines = 224 };

/* One full-frame surface per plane we care about. */
static uint8_t g_frame[kPitch * kLines];
static uint8_t g_bg1_primary[kPitch * kLines];
static uint8_t g_bg1_band1[kPitch * kLines];
static uint8_t g_obj_primary[kPitch * kLines];
static uint8_t g_obj_band[3][kPitch * kLines];

static uint32_t PixelAt(const uint8_t *surface, int x, int y) {
    uint32_t out;
    memcpy(&out, surface + (size_t)y * kPitch + (size_t)x * 4, 4);
    return out;
}

/* Runs one whole visible frame. ppu_runLine(y) renders scanline y-1 into the
 * output, so a full frame is lines 0..kLines. */
static void RunFrame(Ppu *ppu) {
    for (int line = 0; line <= kLines; line++)
        ppu_runLine(ppu, line);
}

/* Mode 1, BG1 only: tile columns 0..15 use a priority-0 tilemap entry and
 * columns 16..31 a priority-1 entry (tilemap bit 13), so a single scanline
 * carries both ranks and the split is observable at fixed x positions. */
static void SetUpBg1(Ppu *ppu) {
    ppu->bgmode = 1;
    ppu->screenEnabled[0] = 0x01;             /* BG1 on main screen */
    ppu->bgXsc[0] = 0;                        /* tilemap at VRAM word 0 */
    ppu->bgTileAdr = 0x1;                     /* BG1 tiles at word 0x1000 */
    ppu->inidisp = 0x0f;                      /* full brightness */

    for (int tx = 0; tx < 32; tx++)
        for (int ty = 0; ty < 32; ty++)
            ppu->vram[(ty << 5) + tx] = (uint16_t)(1 | (tx >= 16 ? 0x2000 : 0));

    /* Character 1: every pixel colour index 1 (bitplane 0 all ones). */
    for (int row = 0; row < 8; row++) {
        ppu->vram[(0x1000 + 1 * 16 + row) & 0x7fff] = 0x00ff;
        ppu->vram[(0x1000 + 1 * 16 + 8 + row) & 0x7fff] = 0x0000;
    }
    ppu->cgram[1] = 0x7fff;                   /* palette 0, colour 1 = white */
}

/* Four 8x8 sprites at x = 0/16/32/48, OAM priorities 0..3 respectively. */
static void SetUpSprites(Ppu *ppu) {
    ppu->obsel = 0;                           /* size pair 8x8 / 16x16 */
    for (int slot = 0; slot < 128; slot++)
        ppu->oam[slot * 2] = 0xf000;          /* park every sprite offscreen */
    for (int slot = 0; slot < 4; slot++) {
        ppu->oam[slot * 2] = (uint16_t)(slot * 16);        /* x, y = 0 */
        /* Attribute word: tile low 8 bits at 0-7, tile-high/name-table at 8,
         * palette at 9-11, PRIORITY AT 12-13, flips at 14-15. (Putting the
         * priority in the palette field is an easy mistake and shows up as
         * "every sprite is priority 0".) */
        ppu->oam[slot * 2 + 1] = (uint16_t)(0x0002 | (slot << 12));
    }
    for (int row = 0; row < 8; row++) {
        ppu->vram[(2 * 16 + row) & 0x7fff] = 0x00ff;
        ppu->vram[(2 * 16 + 8 + row) & 0x7fff] = 0x0000;
    }
    /* Sprite palettes start at CGRAM 128; colour index 1 of palette 0. */
    ppu->cgram[128 + 1] = 0x7fff;
}

static void BindBg1Bands(Ppu *ppu) {
    PpuBindOverlaySurface(ppu, kPpuOverlaySource_Bg1, g_bg1_primary, kPitch);
    PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Bg1, 1, g_bg1_band1);
}

static void BindObjBands(Ppu *ppu) {
    PpuBindOverlaySurface(ppu, kPpuOverlaySource_Obj, g_obj_primary, kPitch);
    for (int band = 1; band <= 3; band++)
        PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Obj, band,
                                  g_obj_band[band - 1]);
}

static void TestBgPriorityRouting(void) {
    Ppu *ppu = ppu_init();
    if (!ppu) { g_failures++; return; }
    ppu_reset(ppu);
    PpuBeginDrawing(ppu, g_frame, kPitch, kPpuRenderFlags_NewRenderer);
    SetUpBg1(ppu);
    BindBg1Bands(ppu);
    PpuSetOverlayCapture(ppu, kPpuOverlaySource_Bg1, 0, 0, kPpuXPixels, kLines,
                         kPpuOverlayFlag_RemoveFromGame);
    RunFrame(ppu);

    /* x=8 is inside tile column 1 (priority 0); x=200 is inside column 25
     * (priority 1). Each pixel must appear in exactly ONE surface — the split
     * is a routing decision, not a copy to both. */
    check(PixelAt(g_bg1_primary, 8, 4) != 0,
          "BG1 priority-0 pixel lands in the primary surface");
    check(PixelAt(g_bg1_band1, 8, 4) == 0,
          "BG1 priority-0 pixel does NOT also land in band 1");
    check(PixelAt(g_bg1_band1, 200, 4) != 0,
          "BG1 priority-1 pixel lands in band 1");
    check(PixelAt(g_bg1_primary, 200, 4) == 0,
          "BG1 priority-1 pixel does NOT also land in the primary surface");

    /* RemoveFromGame: the captured layer must be gone from the game frame,
     * which is what lets the host redraw it at another depth without a
     * duplicate showing through. */
    check(PixelAt(g_frame, 8, 4) == PixelAt(g_frame, 200, 4),
          "captured BG1 is removed from the game frame at both priorities");
    ppu_free(ppu);
}

static void TestObjPriorityRouting(void) {
    Ppu *ppu = ppu_init();
    if (!ppu) { g_failures++; return; }
    ppu_reset(ppu);
    PpuBeginDrawing(ppu, g_frame, kPitch, kPpuRenderFlags_NewRenderer);
    ppu->bgmode = 1;
    ppu->inidisp = 0x0f;
    ppu->screenEnabled[0] = 0x10;             /* OBJ on main screen */
    SetUpSprites(ppu);
    BindObjBands(ppu);
    PpuSetOverlayCapture(ppu, kPpuOverlaySource_Obj, 0, 0, kPpuXPixels, kLines,
                         kPpuOverlayFlag_RemoveFromGame);
    PpuSetOverlayOamRange(ppu, 0, 128);
    RunFrame(ppu);

    check(PixelAt(g_obj_primary, 2, 2) != 0,
          "OBJ priority-0 sprite lands in the primary surface");
    for (int band = 1; band <= 3; band++) {
        int x = band * 16 + 2;
        char message[96];
        snprintf(message, sizeof message,
                 "OBJ priority-%d sprite lands in band %d", band, band);
        check(PixelAt(g_obj_band[band - 1], x, 2) != 0, message);
        snprintf(message, sizeof message,
                 "OBJ priority-%d sprite is not also in the primary surface",
                 band);
        check(PixelAt(g_obj_primary, x, 2) == 0, message);
    }
    ppu_free(ppu);
}

static void TestLazyClearAndStaleness(void) {
    Ppu *ppu = ppu_init();
    if (!ppu) { g_failures++; return; }
    ppu_reset(ppu);
    PpuBeginDrawing(ppu, g_frame, kPitch, kPpuRenderFlags_NewRenderer);
    SetUpBg1(ppu);
    BindBg1Bands(ppu);

    /* One active frame writes content into both surfaces. */
    PpuSetOverlayCapture(ppu, kPpuOverlaySource_Bg1, 0, 0, kPpuXPixels, kLines,
                         kPpuOverlayFlag_RemoveFromGame);
    RunFrame(ppu);
    check(PixelAt(g_bg1_primary, 8, 4) != 0 &&
          PixelAt(g_bg1_band1, 200, 4) != 0,
          "active frame populates primary and band surfaces");

    /* Capture withdrawn: the very next frame must fully clear BOTH surfaces,
     * primary and band, including the LAST scanline (a height-agnostic clear
     * is why the dirty countdown is retired at a frame's first line rather
     * than on a hardcoded line 223 — a 240-line frame would otherwise leave
     * rows 224..239 stale). */
    PpuClearOverlayCaptures(ppu);
    RunFrame(ppu);
    check(PixelAt(g_bg1_primary, 8, 4) == 0,
          "primary surface is cleared the frame after capture stops");
    check(PixelAt(g_bg1_band1, 200, 4) == 0,
          "band surface is cleared the frame after capture stops");
    check(PixelAt(g_bg1_primary, 8, kLines - 1) == 0 &&
          PixelAt(g_bg1_band1, 200, kLines - 1) == 0,
          "the final scanline is cleared too, not just the early ones");

    /* Now that both surfaces are known-clean and the capture is still
     * inactive, the clear must actually be SKIPPED — that is the whole point
     * of the optimization. Poison a pixel and prove it survives a frame. */
    uint32_t poison = 0xdeadbeefu;
    memcpy(g_bg1_primary + (size_t)4 * kPitch + 8 * 4, &poison, 4);
    memcpy(g_bg1_band1 + (size_t)4 * kPitch + 200 * 4, &poison, 4);
    RunFrame(ppu);
    check(PixelAt(g_bg1_primary, 8, 4) == poison,
          "an idle bound surface is not re-cleared every frame (primary)");
    check(PixelAt(g_bg1_band1, 200, 4) == poison,
          "an idle bound surface is not re-cleared every frame (band)");

    /* Re-arming the capture must clear the poison rather than composite over
     * it — a surface whose contents are unknown is always cleared first. */
    PpuSetOverlayCapture(ppu, kPpuOverlaySource_Bg1, 0, 0, kPpuXPixels, kLines,
                         kPpuOverlayFlag_RemoveFromGame);
    RunFrame(ppu);
    check(PixelAt(g_bg1_band1, 8, 4) == 0,
          "re-arming the capture clears stale pixels outside the new content");
    ppu_free(ppu);
}

static void TestBindContracts(void) {
    Ppu *ppu = ppu_init();
    if (!ppu) { g_failures++; return; }
    ppu_reset(ppu);

    /* A band cannot exist without its primary. */
    check(!PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Bg1, 1,
                                     g_bg1_band1),
          "binding a band without a primary is rejected");
    check(PpuBindOverlaySurface(ppu, kPpuOverlaySource_Bg1, g_bg1_primary,
                                kPitch),
          "primary binds");
    check(PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Bg1, 1,
                                    g_bg1_band1),
          "band binds once the primary exists");
    /* Band indices are 1..3; 0 and 4 are not bands. */
    check(!PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Bg1, 0,
                                     g_bg1_band1),
          "band 0 is rejected");
    check(!PpuBindOverlayPrioSurface(ppu, kPpuOverlaySource_Bg1, 4,
                                     g_bg1_band1),
          "band 4 is rejected");
    /* Rebinding the primary drops the band family, so callers must re-declare
     * bands after their primary every frame. parallax.c relies on this. */
    PpuBindOverlaySurface(ppu, kPpuOverlaySource_Bg1, g_bg1_primary, kPitch);
    check(ppu->overlayRenderBands[kPpuOverlaySource_Bg1][0] == NULL,
          "rebinding a primary drops its priority bands");
    ppu_free(ppu);
}

int main(void) {
    memset(g_frame, 0, sizeof g_frame);
    TestBgPriorityRouting();
    TestObjPriorityRouting();
    TestLazyClearAndStaleness();
    TestBindContracts();
    if (g_failures) {
        fprintf(stderr, "ppu_overlay_prio_test: %d failure(s)\n", g_failures);
        return 1;
    }
    puts("ppu_overlay_prio_test: PASS");
    return 0;
}
