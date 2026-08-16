#ifndef SNESRECOMP_WS_SHADOW_H
#define SNESRECOMP_WS_SHADOW_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// Presentation-only, world-keyed BG tilemap storage for streaming games.
// Games opt a layer in each frame; the authentic 256-pixel region always
// continues to read real VRAM. Supports 8x8 and 16x16 (BGMODE big-tile)
// layers, and both 64-wide and streamed 32-wide tilemaps.
enum {
  kWsShadowXTiles = 4096,
  kWsShadowYTiles = 512,
};

void WsShadowReset(void);

/* Sparse host-presentation snapshot. Native SNES save states do not contain
 * the world-keyed margin cache, so reloading used to erase the exact history
 * that produced a live widescreen artifact. Games with a versioned extra
 * save chunk can append this opaque blob and restore it before their
 * post-load hook runs. The format is bounds-checked and self-describing;
 * diagnostic provenance pixels are deliberately excluded. */
size_t WsShadowSnapshotSize(void);
bool WsShadowSnapshotSave(void *data, size_t size);
bool WsShadowSnapshotLoad(const void *data, size_t size);

/* True world-space camera origin for shadow keys (tile capture / margins).
 * For games where PPU scroll == camera (SMW-style), pass hScroll/vScroll.
 * For strip-streaming games (Metal Warriors), pass the WRAM camera and also
 * call WsShadowSetScroll with the PPU buffer scroll. */
void WsShadowSetWorld(int layer, uint32_t worldX, uint32_t worldY);

/* PPU scroll used to address the streaming VRAM tilemap window and to convert
 * scanline-wrapped Y back to world Y. Defaults to the SetWorld values. */
void WsShadowSetScroll(int layer, uint32_t scrollX, uint32_t scrollY);

void WsShadowSetBlankTile(int layer, int blankEntry);

// Periodic-fold mode: for layers whose content is horizontally periodic
// (typical parallax backdrops), margin tiles are folded to the congruent
// column inside the native 32-column window. The period is re-detected
// from the natively displayed columns every frame, so folded margins can
// never be stale and never expose unwritten map regions; rows with no
// exact period keep the plain map-wrap fallback. Mutually exclusive with
// WsShadowSetWorld for the same layer (the last registration wins).
void WsShadowSetPeriodicFold(int layer);

/* Permit a widened 64-column tilemap layer to serve the exact map entry the
 * PPU already resolved outside the native 256-pixel viewport. This is for
 * independently-authored parallax planes whose second tilemap screen is
 * valid content, not a circular streaming buffer. It is deliberately
 * accounted separately from RawFallback so enabling it cannot hide an
 * unsafe terrain-ring miss from regression tooling. */
void WsShadowSetRawContinuation(int layer, bool enabled);

/* Keep world-keyed entries across frames instead of clearing every present.
 * The viewport capture still overwrites in-view tiles each frame, so only
 * off-view margins read history. History is dropped automatically when the
 * layer's tilemap base changes (room/scene switch). Off by default. */
void WsShadowSetRetainHistory(int layer, bool retain);

/* Total tilemap columns to capture per frame (default: the 256px viewport
 * plus one fine-scroll overhang column). Games that draw extra valid columns
 * beyond the viewport (streaming headroom) can widen this so margins pick
 * them up. Clamped to the 32/64-column map width. */
void WsShadowSetCaptureCols(int layer, int totalCols);

/* How many tile columns west of the live strip retainHistory keeps (default
 * 12 ≈ 192px at 16×16). Widescreen games should set this to the margin
 * budget in tiles (+ small slop) so left-gutter props despawn once they
 * leave the wide viewport instead of lingering for a full native buffer. */
void WsShadowSetWestKeep(int layer, int tiles);

/* Mirror of WestKeep for the EAST side. Defaults to 0, which reproduces the
 * original behaviour exactly (all history east of the live strip is pruned
 * every frame) so no existing game changes.
 *
 * Set this for a game that scrolls LEFT: east is then the TRAILING edge, and
 * with the default prune its history is discarded before the trailing gutter
 * can ever be served from it. Measured on Mega Man X2 — walking right, the
 * west gutter was served from history; walking left, the east gutter was
 * pixel-identical to no-history at all. */
void WsShadowSetEastKeep(int layer, int tiles);

/* Always-on margin lookup accounting, split by side. Cumulative since process
 * start; never armed. Without this, "the gutter looks unchanged" cannot be
 * told apart from "history was never consulted" or "consulted and missed". */
typedef struct WsShadowMarginStat {
  uint64_t westHit, westMiss, eastHit, eastMiss;
  /* PrefillTile accounting: cells seeded first-time, and guess-origin cells
   * rewritten because the game's CPU-side map re-resolved differently (a
   * nonzero refresh count is the signature of prefill racing a still-
   * streaming room map, e.g. on a clean-launch first widescreen frame). */
  uint64_t prefillSeed, prefillRefresh;
  /* Source selected after a miss. rawFallback is the dangerous case for a
   * rolling tilemap: the renderer consumed wrapped VRAM because no exact,
   * folded, or verified-blank source was available. */
  uint64_t westFold, eastFold;
  uint64_t westBlank, eastBlank;
  uint64_t westRawContinuation, eastRawContinuation;
  uint64_t westRawFallback, eastRawFallback;
  /* Scene-local window accounting. The store is indexed by world tile MINUS
   * a per-layer aligned origin that follows the camera, so high absolute
   * world coordinates (bonus stages parked deep in the coordinate map) stay
   * cacheable. outOfRange* count keys that still fell outside the rebased
   * window — nonzero means content was lost and must be investigated, never
   * shrugged off as a generic margin miss. originRebase counts window moves.
   * Set SNESRECOMP_WS_CACHE_LOG=<path> for a per-event jsonl detail log. */
  uint64_t outOfRangeRead, outOfRangeWrite, originRebase;
  /* Stream retrodiction: a generated margin entry (prefill guess or exact
   * decoder fill) that was actually SERVED to the renderer is compared
   * against the game's own native capture when that world tile later
   * enters the view. A mismatch proves wrong art was shown in the margin
   * — decoder bug, stale cache, or world-key error — with the game itself
   * as the oracle. Set SNESRECOMP_WS_RETRODICT=<path> for per-event jsonl. */
  uint64_t retrodictChecked, retrodictMismatch;
} WsShadowMarginStat;
void WsShadowGetMarginStats(int layer, WsShadowMarginStat *out);

/* Host-only margin provenance capture. Disabled by default and never part of
 * emulated or serialized state. When armed, the PPU's background renderers
 * record the exact source selected for each widened-margin pixel. */
typedef enum WsShadowProvenance {
  kWsShadowProvenanceNone = 0,
  kWsShadowProvenanceCaptured,
  kWsShadowProvenancePrefill,
  kWsShadowProvenanceFold,
  kWsShadowProvenanceBlank,
  kWsShadowProvenanceRawContinuation,
  kWsShadowProvenanceRawFallback,
  /* The margin's world key fell outside the scene-local cache window; the
   * art shown came from a fallback that cannot be world-correct. */
  kWsShadowProvenanceOutOfRange,
} WsShadowProvenance;
void WsShadowDebugSetProvenanceEnabled(bool enabled);
bool WsShadowDebugProvenanceEnabled(void);
void WsShadowDebugBeginFrame(void);
uint8_t WsShadowDebugProvenanceAt(int layer, int screenX, int screenY);

/* Current scene-local origins (in tiles of the layer's tile size). Purely
 * diagnostic; world-keyed public APIs are unaffected by rebasing. */
void WsShadowDebugOrigin(int layer, long long *originTx, long long *originTy);

/* Per-cell last-writer attribution for the click-to-provenance inspector.
 * Armed by SNESRECOMP_WS_WRITE_TRACE=1 (lazy, ~10 MB/layer). Returns 0
 * when unarmed or the cell has no recorded writer; else a nonzero kind and
 * the snes frame of the last write. Attribution is cleared on window
 * rebase/scene reset (diagnostic data only). */
typedef enum WsShadowWriteKind {
  kWsShadowWriteNone = 0,
  kWsShadowWriteViewCapture,   /* native view sweep from real VRAM */
  kWsShadowWriteUploadMirror,  /* game's own VRAM upload (OnVramWrite) */
  kWsShadowWriteDecodeForce,   /* exact ROM decoder fill (ForceTile) */
  kWsShadowWritePrefillGuess,  /* CPU-side map guess (PrefillTile) */
  kWsShadowWriteWestViewport,  /* west strip keyed by viewport row */
  kWsShadowWriteBackfill,      /* backfill/shift/gap heuristics */
} WsShadowWriteKind;
int WsShadowDebugLastWriter(int layer, uint32_t worldTileX,
                            uint32_t worldTileY, uint32_t *frame);
const char *WsShadowWriteKindName(int kind);

/* Read-only diagnostic lookup in the world-keyed store. This does not alter
 * hit/miss counters or renderer state. It lets offline route audits compare
 * the exact tilemap entry served in a margin with the entry later captured
 * when that same world cell reaches the native viewport. */
bool WsShadowLookupWorldTile(int layer, uint32_t worldTileX,
                             uint32_t worldTileY, uint16_t *entry);

/* Debug/observability: read one shadow cell without touching stats.
 * Returns 0 = cell invalid, 1 = captured from real VRAM (authoritative),
 * 2 = prefill guess (still refreshable). *entry is set for 1/2. */
int WsShadowDebugCell(int layer, uint32_t worldTileX, uint32_t worldTileY,
                      uint16_t *entry);

/* When set, capture columns east of the 256px view that match any live
 * view column are cleared instead of stored — kills VRAM-wrap / period
 * phantoms (e.g. a second door) in the right widescreen gutter. */
void WsShadowSetRejectEastEcho(int layer, bool reject);

/* Separate the cartridge-authentic viewport captured from VRAM from the
 * host presentation viewport used for margin lookup. Most callers leave
 * these equal via WsShadowSetWorld. A host that shifts presentation near a
 * level boundary must override the capture origin so stale ring headroom is
 * not mislabeled as authoritative world data. */
void WsShadowSetCaptureWorld(int layer, uint32_t worldX, uint32_t worldY);

/* Pixels removed from the authentic native source interval by a host-only
 * presentation shift. Tile chunks touching either inset are resolved from
 * world-keyed shadow data even though their destination X is still 0..255. */
void WsShadowSetNativeViewportInset(int layer, int leftPixels,
                                    int rightPixels);

// Supply a raw tilemap entry resolved from the game's CPU-side map for a
// world tile. This is useful when a game retains full room data in WRAM but
// streams only the native viewport to VRAM. It changes renderer-side state
// only. Never overwrites an entry that Frame/history/OnVramWrite captured
// from real VRAM; it MAY refresh a cell it seeded itself when a later call
// resolves a different value (the game's map was still streaming when the
// first guess was taken — without this, a clean-launch race freezes garbage
// into the margin until the native view sweeps it).
void WsShadowPrefillTile(int layer, uint32_t worldTileX, uint32_t worldTileY,
                         uint16_t entry);

/* Like PrefillTile but always writes (DMA-pad VRAM beats $7F guesses). */
void WsShadowForceTile(int layer, uint32_t worldTileX, uint32_t worldTileY,
                       uint16_t entry);

/* Record an authoritative tile from the game's live rolling map. Unlike
 * ForceTile this is captured provenance (not generated/prefilled data) and
 * participates in retrodiction checks exactly like the ordinary viewport
 * sweep. Hosts with checksum-verified wider cartridge streams can use this
 * to ingest valid columns on both sides of the native viewport. */
void WsShadowCaptureTile(int layer, uint32_t worldTileX, uint32_t worldTileY,
                         uint16_t entry);

/* ForceTile yields to world cells the game itself wrote (via
 * WsShadowOnVramWrite) within the last `frames` frames (0 = off, max 250).
 * Required by games that draw moving objects INTO the BG tilemap
 * (platforms, elevators): widened object windows make the game draw them
 * in the margins, and an exact margin refill would erase them at the
 * native boundary on the very next frame. */
void WsShadowSetRespectGameWrites(int layer, int frames);

/* retainHistory layers only: force a west-of-view tile at a viewport-row Y
 * key (not world Y). Same ownership model as live capture: every present
 * overwrites. Games call this with strip memory west of the DMA base so the
 * left gutter tracks current source like the 4:3 strip / right headroom.
 * worldTileX must be < tx0. */
void WsShadowForceWestViewportTile(int layer, uint32_t worldTileX,
                                   uint32_t viewportRow, uint16_t entry);

/* Like ForceWestViewportTile but only fills missing cells. Use when the ROM
 * strip base barely moves with the camera — Force every present would drag
 * the same west decoration through world space (sticky left chains). */
void WsShadowPrefillWestViewportTile(int layer, uint32_t worldTileX,
                                     uint32_t viewportRow, uint16_t entry);

/* Drop a world-keyed entry (e.g. reject stale DMA-pad echo columns). */
void WsShadowInvalidateTile(int layer, uint32_t worldTileX,
                            uint32_t worldTileY);

/* After WsShadowFrame: fill still-missing margin columns by repeating the
 * nearest captured viewport-edge column. Used when the game only streams a
 * 256px strip and no CPU-side map buffer is available to prefill from.
 * marginPixels is the per-side widescreen budget (e.g. g_ws_extra). */
void WsShadowExtendEdges(int layer, int marginPixels);

/* Like ExtendEdges, but searches inward for the nearest non-zero tile and
 * fills missing/zero margin cells (and zero overhang) so layers continue
 * into the gutter under transparent holes. */
void WsShadowExtendSolidEdges(int layer, int marginPixels);

/* BG1 seam only: copy the last captured view column into the first missing
 * column past the view (at most one tile each side). No full-gutter smear. */
void WsShadowContinueSeam(int layer);

// Capture the known-good native viewport for later margin use.
struct Ppu;
void WsShadowFrame(const struct Ppu *ppu);

// Feed a VRAM word write (post-merge value) from the emulation's write
// paths. Writes landing inside a registered wide layer's tilemap are
// captured into the world-keyed history, bound to the world chunk the
// upload was staged for (half parity + travel direction). No-op for
// inactive layers/addresses.
void WsShadowOnVramWrite(uint16_t wordAdr, uint16_t value);

// mapWordAdr = the VRAM word address the renderer fetched realTile from
// (used by fold mode to recover the exact map row/column, independent of
// scroll bias and window splits). hScroll = the layer's live per-line
// scroll: parallax strips change it mid-frame, so fold anchoring must
// use the value the renderer used for THIS line, never a frame sample.
uint16_t WsShadowTile(int layer, int screenX, uint32_t wrappedY,
                      uint16_t hScroll, uint16_t mapWordAdr,
                      uint16_t realTile);
uint16_t WsShadowTileDebug(int layer, int screenX, int screenY, int pixelSpan,
                           uint32_t wrappedY, uint16_t hScroll,
                           uint16_t mapWordAdr, uint16_t realTile);
bool WsShadowLayerActive(int layer);

/* Latched world/scroll origins for margin pixel-phase (must match tile keys). */
uint32_t WsShadowWorldX(int layer);
uint32_t WsShadowWorldY(int layer);
/* Presentation Y for a margin screenX (west may use a frozen follow origin). */
uint32_t WsShadowPresentWorldY(int layer, int screenX);
uint32_t WsShadowScrollX(int layer);
uint32_t WsShadowScrollY(int layer);

#endif
