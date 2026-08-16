/* Scene-local cache window regression for ws_shadow.
 *
 * The DKC1 bonus-stage class: absolute world tile X above the flat store's
 * 4096-tile limit was silently unstorable — ForceTile dropped the write,
 * margin lookups missed, and the gutter fell back to wrong art with no
 * counter to show why. The store now rebases a per-layer aligned origin so
 * high absolute coordinates stay cacheable, and any key that still falls
 * outside the window is accounted, never shrugged off.
 *
 * No ROM, PPU, or frontend required: the world-keyed write/lookup surface
 * and the stats are enough to pin the contract.
 */
#include <inttypes.h>
#include <stdio.h>

#include "snes/ppu.h"
#include "snes/ws_shadow.h"

/* ws_shadow references this; the PPU accessors are macros over struct
 * fields and are not exercised without WsShadowFrame. */
int snes_frame_counter;

static int g_failures;

static void Check(int condition, const char *what) {
  if (condition) {
    printf("ok   %s\n", what);
  } else {
    printf("FAIL %s\n", what);
    g_failures++;
  }
}

int main(void) {
  WsShadowReset();
  WsShadowMarginStat stat;

  /* A bonus-stage-like world: camera parked at 40000px => tile 5000 at
   * 8x8, beyond the flat store's historical 4096-tile ceiling. */
  WsShadowSetWorld(0, 40000, 1000);
  const uint32_t tile_x = 40000u >> 3; /* 5000 */
  const uint32_t tile_y = 1000u >> 3;  /* 125 */
  for (uint32_t i = 0; i < 8; i++)
    WsShadowForceTile(0, tile_x + i, tile_y, (uint16_t)(0x1000 + i));

  uint16_t entry = 0;
  int stored = 1;
  for (uint32_t i = 0; i < 8; i++) {
    if (!WsShadowLookupWorldTile(0, tile_x + i, tile_y, &entry) ||
        entry != (uint16_t)(0x1000 + i))
      stored = 0;
  }
  Check(stored, "high-X world tiles are stored and served back");

  WsShadowGetMarginStats(0, &stat);
  Check(stat.outOfRangeWrite == 0,
        "no out-of-range writes for camera-local high-X tiles");

  long long origin_tx = 0, origin_ty = 0;
  WsShadowDebugOrigin(0, &origin_tx, &origin_ty);
  Check(origin_tx > 0, "origin rebased away from zero for a high-X world");
  Check(origin_tx % (512 / 8) == 0, "X origin preserves 512px alignment");
  Check(origin_ty % (256 / 8) == 0, "Y origin preserves 256px alignment");

  /* Travel far enough right to force a window rebase; earlier content that
   * is still inside the follow window must survive the shift. */
  WsShadowSetWorld(0, 40000 + 14400, 1000);
  WsShadowForceTile(0, (40000u + 14400u) >> 3, tile_y, 0x2222);
  WsShadowGetMarginStats(0, &stat);
  Check(stat.originRebase >= 1, "window rebased while traveling");
  Check(WsShadowLookupWorldTile(0, tile_x, tile_y, &entry) &&
            entry == 0x1000,
        "pre-rebase content survives an aligned window shift");
  Check(WsShadowLookupWorldTile(0, (40000u + 14400u) >> 3, tile_y, &entry) &&
            entry == 0x2222,
        "post-rebase content stored at the new position");

  /* A key nowhere near the window reads as absent, not as wrapped art. */
  Check(!WsShadowLookupWorldTile(0, 10, tile_y, &entry),
        "far-outside key misses instead of aliasing");

  /* Low-coordinate worlds keep the identity mapping (origin 0), so every
   * in-range game behaves exactly as the pre-rebase engine. */
  WsShadowReset();
  WsShadowSetWorld(1, 512, 256);
  WsShadowForceTile(1, 512u >> 3, 256u >> 3, 0x0777);
  WsShadowDebugOrigin(1, &origin_tx, &origin_ty);
  Check(origin_tx == 0 && origin_ty == 0,
        "low-coordinate world keeps origin zero (identity mapping)");
  Check(WsShadowLookupWorldTile(1, 512u >> 3, 256u >> 3, &entry) &&
            entry == 0x0777,
        "identity-mapped store serves entries");

  if (g_failures) {
    printf("%d failure(s)\n", g_failures);
    return 1;
  }
  printf("all scene-local cache checks passed\n");
  return 0;
}
