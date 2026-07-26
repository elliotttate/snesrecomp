#ifndef SNESRECOMP_PARALLAX_H
#define SNESRECOMP_PARALLAX_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include <SDL.h>

#include "snes/ppu.h"

/* ── Layered pseudo-3D parallax presenter (game-agnostic) ─────────────────
 *
 * Splits the composited SNES frame back into its source layers via the PPU's
 * host-overlay extraction (PpuBindOverlaySurface / PpuBindOverlayPrioSurface,
 * snes/ppu.h), then re-composites them as textured quads at different world
 * depths under a shared perspective camera. Nearer layers sweep faster than
 * far ones, so the flat 2D scene reads as a shallow diorama.
 *
 * Attribution: this is a generalization of the "diorama" presentation in
 * DerrickGold/ar-recomp (an ActRaiser recompilation built on snesrecomp).
 * ar-recomp hardcoded ActRaiser's Mode-1 layer stack in one table; here the
 * per-game layer stack is DATA (ParallaxProfile) supplied by the game's
 * frontend, and everything else — capture policy, camera, projection,
 * compositing — is shared. See docs/PARALLAX.md and IMPROVEMENTS.md.
 *
 * Requirements a game must meet to opt in:
 *   * the priority-buffer PPU (kPpuRenderFlags_NewRenderer / the widescreen
 *     path). The legacy pixel-at-a-time renderer has no overlay capture.
 *   * an SDL_Renderer present path. The OpenGL backend is not supported;
 *     Parallax_Composite reports failure and the caller presents flat.
 *   * BG mode 1 or 7 for the frames it wants split (the only modes with
 *     overlay capture wired in ppu.c).
 *
 * Default behavior with no profile set and enabled=false is a deterministic
 * no-op: nothing is bound, nothing is captured, and the frame presents exactly
 * as it does today.
 */

/* Plane indices. The first kPpuOverlaySource_Count deliberately ARE the PPU
 * overlay source indices, so a plane id can be used directly as a source. The
 * appended entries are the priority-band splits (PpuBindOverlayPrioSurface)
 * plus the backdrop slot, which receives the residual game framebuffer. */
enum {
  kParallaxPlane_Bg1 = kPpuOverlaySource_Bg1,  /* BG1, priority-0 tiles */
  kParallaxPlane_Bg2 = kPpuOverlaySource_Bg2,
  kParallaxPlane_Bg3 = kPpuOverlaySource_Bg3,
  kParallaxPlane_Bg4 = kPpuOverlaySource_Bg4,
  kParallaxPlane_Obj = kPpuOverlaySource_Obj,  /* sprites, priority 0 */
  kParallaxPlane_Backdrop = kPpuOverlaySource_Count, /* residual main frame */
  kParallaxPlane_Bg1Hi,                        /* BG1, priority-1 tiles */
  kParallaxPlane_Bg2Hi,
  kParallaxPlane_Bg3Hi,
  kParallaxPlane_Obj1,                         /* sprites, priority 1 */
  kParallaxPlane_Obj2,
  kParallaxPlane_Obj3,
  kParallaxPlane_Count
};

/* Visibility/tuning groups. Several planes share one group (a layer and its
 * priority band; all four sprite bands) so a layer can never parallax-split
 * against itself, and one toggle hides the whole layer. */
typedef enum ParallaxGroup {
  kParallaxGroup_Backdrop = 0,
  kParallaxGroup_Bg1,
  kParallaxGroup_Bg2,
  kParallaxGroup_Bg3,
  kParallaxGroup_Bg4,
  kParallaxGroup_Obj,
  kParallaxGroup_Count
} ParallaxGroup;

typedef struct ParallaxPlaneDesc {
  uint8_t plane;        /* kParallaxPlane_* */
  ParallaxGroup group;  /* visibility group */
  float z;              /* 0.0 = farthest .. 1.0 = nearest */
  float shade_r, shade_g, shade_b;  /* depth tint at full strength; 1 = none */
  bool casts_shadow;    /* drop a soft offset silhouette on layers behind */
} ParallaxPlaneDesc;

/* A game's layer stack. `planes` order IS the draw order (painter's
 * algorithm) and should mirror the game's SNES priority stack so occlusion
 * matches hardware. `capture_mask` is the set of PpuOverlaySources to capture
 * (bit L = source L); a source that is never captured leaves the layer in the
 * residual framebuffer, where it becomes part of the backdrop plane. */
typedef struct ParallaxProfile {
  const char *name;
  const ParallaxPlaneDesc *planes;
  int plane_count;
  uint8_t capture_mask;
  /* Bands to bind, as (source, band) pairs encoded plane-side: for each entry
   * in `planes` whose id is a *Hi/*Obj1..3 plane, the module derives the
   * (source, band) pair itself — no extra table needed. */
} ParallaxProfile;

/* Runtime state the frontend owns. Games wire `enabled` to their config and
 * may expose the camera fields in a settings UI; everything has a working
 * default so a game can opt in by setting `enabled` alone. */
typedef struct ParallaxSettings {
  bool enabled;         /* master switch (config.ini [Graphics] Parallax) */
  int tilt_x_mrad;      /* pitch, milliradians */
  int tilt_y_mrad;      /* yaw, milliradians */
  int distance_x100;    /* camera distance * 100; 0 = auto-fit the frame */
  int depth_shade;      /* 0..100, how strongly far layers are tinted */
  int layer_gap;        /* 0..200, scales every plane's depth offset */
  /* How strongly the camera leans into the game's own scroll motion
   * (Parallax_ReportCameraMotion), 0..200 where 100 is the tuned default.
   * 0 pins the camera and — with `fill` on — makes the whole effect visually
   * inert, so treat 0 as "off", not "subtle". */
  int sway;
  bool shadows;         /* per-layer offset silhouette */
  bool smooth;          /* supersampled premultiplied edge AA */
  /* Scale each plane by its depth so every layer still fills the frame
   * (default). false gives the literal diorama look — far layers are visibly
   * smaller and the frame's edges show through around them. */
  bool fill;
  bool visible[kParallaxGroup_Count];  /* per-group visibility (debug/tuning) */
} ParallaxSettings;

extern ParallaxSettings g_parallax;

/* Install a game's layer stack. Passing NULL disables the feature outright.
 * Safe to call once at startup; the profile must outlive the process. */
void Parallax_SetProfile(const ParallaxProfile *profile);

/* True iff a profile is installed AND the master switch is on. Cheap; a game
 * can gate its "force the new PPU" decision on this. */
bool Parallax_Enabled(void);

/* GAME THREAD, once per frame, BEFORE the PPU draws the frame (i.e. next to
 * the game's other per-frame PpuSet* policy calls).
 *
 * `frame_width` is the internal render width (256 + 2*extra) and `extra` the
 * per-side widescreen margin, matching what the game passed to PpuBeginDrawing
 * / PpuSetExtraSpace. `active` is the game's own scene gate — parallax only
 * makes sense during actual gameplay, so a game passes false on title
 * screens, maps, cutscenes, or any mode whose layers are not a world.
 *
 * When inactive this unbinds the surfaces, which restores byte-identical
 * authentic composition on the very next frame. */
void Parallax_PrepareFrame(Ppu *ppu, int frame_width, int frame_height,
                           int extra, bool active);

/* Whether the frame just rendered was captured for parallax — the present
 * side's gate. Reflects the last Parallax_PrepareFrame call. */
bool Parallax_IsActiveThisFrame(void);

/* PRESENT THREAD. Uploads the captured planes plus `backdrop` (the residual
 * game framebuffer, `frame_width`*4 bytes per row) into GPU textures, then
 * composites them as depth-separated quads inside `viewport` (the same
 * destination rect the flat path would blit to, so letterboxing and aspect
 * handling stay consistent).
 *
 * Returns false when it could not draw — no profile, not active, no renderer,
 * texture allocation failure. The caller MUST fall back to its normal flat
 * present in that case, so a failure is never a black screen. */
bool Parallax_Composite(SDL_Renderer *renderer, const SDL_Rect *viewport,
                        const uint8_t *backdrop, int frame_width,
                        int frame_height);

/* Release GPU textures (renderer teardown / resolution change). */
void Parallax_DestroyTextures(void);

/* Aspect-preserving destination rect for a frame_w x frame_h frame inside an
 * out_w x out_h output, i.e. the rect a flat blit would letterbox into.
 *
 * Games that already compute their own present rect in real output pixels
 * (MMX's MmxDisplay_ComputeViewport) should pass that instead. This exists for
 * games that delegate letterboxing to SDL_RenderSetLogicalSize: the composite
 * must run at full output resolution to look sharp, so such a game disables
 * logical presentation for the composite and needs the rect computed here.
 * `stretch` fills the output and ignores aspect. */
void Parallax_LetterboxViewport(int out_w, int out_h, int frame_w,
                                int frame_h, bool stretch, SDL_Rect *out);

/* Interactive camera. Deltas are radians / world units; the module clamps to
 * its own safe range and writes the result back into g_parallax so a game's
 * config persistence sees the change. */
void Parallax_AdjustCamera(float d_yaw, float d_pitch, float d_zoom);
void Parallax_ResetCamera(void);

/* Report this frame's camera motion, in SNES pixels, so the presenter can lean
 * the render camera into it.
 *
 * This is the difference between the effect working and being visually inert.
 * With depth-compensated sizing ("fill") and a STATIC camera, every plane
 * subtends the same screen area and the composite collapses to the flat frame
 * plus a keystone — the layers cannot be told apart. Differential motion is the
 * cue that actually reads as depth: leaning the camera by a fraction of the
 * game's own scroll makes near planes sweep further across the screen than far
 * ones, which is exactly what a multiplane camera does.
 *
 * A game passes its per-frame camera delta (dx, dy) — typically the change in
 * whichever BG scroll register or WRAM camera drives its playfield. Sign
 * convention: positive dx = the view moving right. Call once per frame from the
 * same place as Parallax_PrepareFrame; pass 0,0 on a frame with no meaningful
 * delta (scene change, warp, load) so a discontinuity is not mistaken for a
 * huge camera sweep. */
void Parallax_ReportCameraMotion(float dx_px, float dy_px);

/* Suggested per-pixel drag sensitivity and per-notch zoom step, so every
 * game's mouse/gamepad binding feels the same. */
float Parallax_DragRadPerPx(void);
float Parallax_ZoomStep(void);

/* Human-readable one-line state, for an on-screen/stderr debug report. */
void Parallax_DescribeState(char *out, size_t out_size);

#endif  /* SNESRECOMP_PARALLAX_H */
