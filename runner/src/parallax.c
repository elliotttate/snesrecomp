#include "parallax.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "scene3d_math.h"

/* Generalized from DerrickGold/ar-recomp's src/diorama.c — see parallax.h for
 * attribution and the design contract. */

/* SDL_RenderGeometry landed in SDL 2.0.18. Rather than make the whole runner
 * require it (the MSVC solutions pin their own SDL2 package), compile the
 * composite out on older SDL and report "cannot draw" at runtime — the caller
 * already has to handle that and presents flat. Capture policy still builds, so
 * nothing else in the runner needs version-guarding. */
#if SDL_VERSION_ATLEAST(2, 0, 18)
#define PARALLAX_HAVE_RENDER_GEOMETRY 1
#else
#define PARALLAX_HAVE_RENDER_GEOMETRY 0
#endif

/* ── Tunables ─────────────────────────────────────────────────────────── */

static const float kParallaxFovY = 0.4f;
static const float kParallaxTiltMin = -0.7f, kParallaxTiltMax = 0.7f;
static const float kParallaxDistMin = 2.0f, kParallaxDistMax = 20.0f;
static const float kParallaxDragRadPerPx = 0.005f;
static const float kParallaxZoomStep = 0.5f;

/* The plane the camera is framed around: the playfield most games put their
 * action on. Depth offsets are measured from here, so raising layer_gap pushes
 * backgrounds away without moving the gameplay plane off-centre. */
static const float kParallaxFocalZ = 0.5f;

/* Extra size beyond exact depth-compensation, per world unit BEHIND the focal
 * plane, so a tilted far plane's edges still cover the frame.
 *
 * Applied only behind the focal plane, and proportionally: a plane in front of
 * the focal plane never needs it (whatever sits behind it covers any gap its
 * tilt opens), whereas the farthest plane has nothing behind it but the clear
 * colour. A flat overscan across all planes instead CROPS the nearest layer —
 * which for SMW is the status bar / title border, i.e. the one thing that must
 * stay whole. */
static const float kParallaxOverscanPerUnit = 0.70f;

/* Mesh subdivision. A single quad would interpolate UVs linearly across the
 * whole tilted plane, which is visibly wrong under perspective; subdividing
 * lets the renderer approximate the projective warp per cell. 8x6 matches the
 * source implementation and is imperceptibly different from finer grids. */
#define PARALLAX_SUBDIV_X 8
#define PARALLAX_SUBDIV_Y 6
#define PARALLAX_VERTS ((PARALLAX_SUBDIV_X + 1) * (PARALLAX_SUBDIV_Y + 1))
#define PARALLAX_INDICES (PARALLAX_SUBDIV_X * PARALLAX_SUBDIV_Y * 6)

/* Supersample factor for the smooth (premultiplied AA) path. */
enum { kParallaxSupersample = 4 };

/* Captured surfaces are allocated at the maximum internal frame size so a
 * widescreen-width change never reallocates mid-run. */
enum { kParallaxMaxHeight = 240 };

ParallaxSettings g_parallax = {
  .enabled = false,
  .tilt_x_mrad = 120,     /* ~6.9 deg of pitch: enough to read as depth
                             without the keystone dominating the picture */
  .tilt_y_mrad = 0,
  .distance_x100 = 0,     /* auto-fit */
  .depth_shade = 55,
  .layer_gap = 100,
  .sway = 100,
  .shadows = true,
  .smooth = true,
  .fill = true,
  .visible = { true, true, true, true, true, true },
};

/* ── State ────────────────────────────────────────────────────────────── */

static const ParallaxProfile *s_profile;
static bool s_active_this_frame;
static uint8_t *s_plane_pixels[kParallaxPlane_Count];
static SDL_Texture *s_plane_textures[kParallaxPlane_Count];
static int s_texture_width, s_texture_height;
static SDL_Texture *s_supersample;
static int s_supersample_w, s_supersample_h;
static bool s_smooth_unavailable;      /* render-target/blend probe failed */
static SDL_BlendMode s_premultiplied;
static bool s_premultiplied_ready;
static float s_auto_distance = 5.0f;
static int s_bound_pitch;              /* pitch the surfaces are bound at */

/* Scroll-driven camera lean (see Parallax_ReportCameraMotion). s_lean_* are the
 * smoothed, decaying yaw/pitch offsets actually applied to the render camera. */
static float s_lean_yaw, s_lean_pitch;

/* Radians of lean per SNES pixel of camera motion per frame, at sway=100.
 * Sized so ordinary walking speed (~1-2 px/frame) produces a lean that is
 * clearly legible without the world appearing to swing. */
static const float kParallaxLeanRadPerPx = 0.030f;
/* Cap so a screen transition or warp that slips past the caller's own
 * discontinuity check cannot whip the camera to the clamp. */
static const float kParallaxLeanMax = 0.22f;
/* Per-frame decay toward centre when motion stops. Slow enough to glide, fast
 * enough that the world settles rather than drifting. */
static const float kParallaxLeanDecay = 0.90f;
/* Smoothing of the incoming delta, so single-frame scroll jitter (HDMA-driven
 * layers, sub-pixel camera rounding) does not read as a twitch. */
static const float kParallaxLeanSmooth = 0.25f;

static float Clampf(float v, float lo, float hi) {
  return v < lo ? lo : (v > hi ? hi : v);
}

/* ── Plane/source mapping ─────────────────────────────────────────────── */

/* Maps a priority-band plane back to the (source, band) pair that feeds it.
 * Returns false for a primary plane or the backdrop. This is what lets a
 * profile name bands as plain planes and keeps the band table out of games. */
static bool ParallaxPlaneBand(int plane, PpuOverlaySource *out_source,
                              int *out_band) {
  switch (plane) {
    case kParallaxPlane_Bg1Hi: *out_source = kPpuOverlaySource_Bg1; *out_band = 1; return true;
    case kParallaxPlane_Bg2Hi: *out_source = kPpuOverlaySource_Bg2; *out_band = 1; return true;
    case kParallaxPlane_Bg3Hi: *out_source = kPpuOverlaySource_Bg3; *out_band = 1; return true;
    case kParallaxPlane_Obj1:  *out_source = kPpuOverlaySource_Obj; *out_band = 1; return true;
    case kParallaxPlane_Obj2:  *out_source = kPpuOverlaySource_Obj; *out_band = 2; return true;
    case kParallaxPlane_Obj3:  *out_source = kPpuOverlaySource_Obj; *out_band = 3; return true;
    default: return false;
  }
}

static uint8_t *ParallaxEnsurePlaneBuffer(int plane) {
  if (!s_plane_pixels[plane]) {
    s_plane_pixels[plane] =
        (uint8_t *)calloc(1, (size_t)kPpuBufWidth * 4 * kParallaxMaxHeight);
    if (!s_plane_pixels[plane])
      fprintf(stderr, "[parallax] plane %d buffer allocation failed\n", plane);
  }
  return s_plane_pixels[plane];
}

/* ── Public config/camera ─────────────────────────────────────────────── */

void Parallax_SetProfile(const ParallaxProfile *profile) {
  s_profile = profile;
}

bool Parallax_Enabled(void) {
  return s_profile != NULL && g_parallax.enabled;
}

bool Parallax_IsActiveThisFrame(void) {
  return s_active_this_frame;
}

float Parallax_DragRadPerPx(void) { return kParallaxDragRadPerPx; }
float Parallax_ZoomStep(void) { return kParallaxZoomStep; }

void Parallax_AdjustCamera(float d_yaw, float d_pitch, float d_zoom) {
  float tilt_y = Clampf((float)g_parallax.tilt_y_mrad / 1000.0f + d_yaw,
                        kParallaxTiltMin, kParallaxTiltMax);
  float tilt_x = Clampf((float)g_parallax.tilt_x_mrad / 1000.0f + d_pitch,
                        kParallaxTiltMin, kParallaxTiltMax);
  g_parallax.tilt_y_mrad = (int)lrintf(tilt_y * 1000.0f);
  g_parallax.tilt_x_mrad = (int)lrintf(tilt_x * 1000.0f);
  if (d_zoom != 0.0f) {
    /* distance 0 is the auto-fit sentinel, so the first zoom step has to start
     * from the resolved auto distance rather than from 0 — otherwise a single
     * notch lands inside the near plane and clips the whole scene away. */
    float base = g_parallax.distance_x100 > 0
        ? (float)g_parallax.distance_x100 / 100.0f : s_auto_distance;
    g_parallax.distance_x100 =
        (int)lrintf(Clampf(base + d_zoom, kParallaxDistMin, kParallaxDistMax) *
                    100.0f);
  }
}

void Parallax_ReportCameraMotion(float dx_px, float dy_px) {
  float gain = (float)g_parallax.sway / 100.0f;
  if (gain <= 0.0f) {
    s_lean_yaw = s_lean_pitch = 0.0f;
    return;
  }
  /* Target lean for this frame's motion, then a low-pass toward it. The decay
   * is applied to the target (not the state) so a frame with no motion pulls
   * the lean back toward centre instead of freezing it where it was. */
  float target_yaw = Clampf(dx_px * kParallaxLeanRadPerPx * gain,
                            -kParallaxLeanMax, kParallaxLeanMax);
  float target_pitch = Clampf(dy_px * kParallaxLeanRadPerPx * gain,
                              -kParallaxLeanMax, kParallaxLeanMax);
  s_lean_yaw += (target_yaw - s_lean_yaw) * kParallaxLeanSmooth;
  s_lean_pitch += (target_pitch - s_lean_pitch) * kParallaxLeanSmooth;
  s_lean_yaw *= kParallaxLeanDecay;
  s_lean_pitch *= kParallaxLeanDecay;
}

void Parallax_ResetCamera(void) {
  s_lean_yaw = s_lean_pitch = 0.0f;
  g_parallax.tilt_x_mrad = 120;
  g_parallax.tilt_y_mrad = 0;
  g_parallax.distance_x100 = 0;
  g_parallax.depth_shade = 55;
  g_parallax.layer_gap = 100;
  for (int i = 0; i < kParallaxGroup_Count; i++)
    g_parallax.visible[i] = true;
}

void Parallax_DescribeState(char *out, size_t out_size) {
  if (!out || !out_size) return;
  snprintf(out, out_size,
           "parallax %s profile=%s pitch=%.3f yaw=%.3f dist=%s shade=%d "
           "gap=%d shadows=%d smooth=%d",
           Parallax_Enabled() ? "on" : "off",
           s_profile && s_profile->name ? s_profile->name : "(none)",
           (float)g_parallax.tilt_x_mrad / 1000.0f,
           (float)g_parallax.tilt_y_mrad / 1000.0f,
           g_parallax.distance_x100 > 0 ? "manual" : "auto",
           g_parallax.depth_shade, g_parallax.layer_gap,
           (int)g_parallax.shadows, (int)g_parallax.smooth);
}

/* ── Capture policy (game thread) ─────────────────────────────────────── */

/* SNESRECOMP_PARALLAX_FORCE=1 overrides the game's scene gate, so EVERY frame
 * the PPU can split gets captured and composited — title screens, menus, maps
 * included. Those frames are not worlds and will look wrong; that is the
 * point. It separates the two things a "parallax isn't showing" report can
 * mean ("compositing is broken" vs "the game's gameplay gate never fired"),
 * which are otherwise indistinguishable from the outside and cost a whole
 * debugging session to tell apart. Diagnostics only; never a shipped default. */
static bool ParallaxForceActive(void) {
  static int checked;
  static bool forced;
  if (!checked) {
    checked = 1;
    const char *v = getenv("SNESRECOMP_PARALLAX_FORCE");
    forced = v && *v && *v != '0';
    if (forced)
      fprintf(stderr, "[parallax] SNESRECOMP_PARALLAX_FORCE=1 — ignoring the "
                      "game's scene gate (diagnostics)\n");
  }
  return forced;
}

void Parallax_PrepareFrame(Ppu *ppu, int frame_width, int frame_height,
                           int extra, bool active) {
  if (!ppu) return;
  if (ParallaxForceActive())
    active = true;
  bool want = Parallax_Enabled() && active && frame_width >= kPpuXPixels &&
              frame_width <= kPpuBufWidth && frame_height > 0 &&
              frame_height <= kParallaxMaxHeight;
  s_active_this_frame = false;
  if (!want) {
    /* Unbinding is what restores authentic composition: an unbound source is
     * never captured, never removed from the game frame, and (via ppu.c's
     * lazy clear) stops costing per-scanline memsets. Only touch the sources
     * this module owns, and only when something is actually bound, so a game
     * with unrelated overlay consumers is unaffected. */
    if (s_bound_pitch) {
      for (int src = 0; src < kPpuOverlaySource_Count; src++)
        if (s_profile && (s_profile->capture_mask & (1u << src)))
          PpuBindOverlaySurface(ppu, (PpuOverlaySource)src, NULL, 0);
      s_bound_pitch = 0;
    }
    return;
  }

  size_t pitch = (size_t)frame_width * 4;

  /* Primaries first: binding a primary drops its band family (ppu.c), so
   * bands must be (re)bound after. Both are re-declared every frame, which
   * makes the whole policy self-healing regardless of toggle history. */
  for (int src = 0; src < kPpuOverlaySource_Count; src++) {
    if (!(s_profile->capture_mask & (1u << src)))
      continue;
    uint8_t *buffer = ParallaxEnsurePlaneBuffer(src);
    if (!buffer)
      continue;
    if (!PpuBindOverlaySurface(ppu, (PpuOverlaySource)src, buffer, pitch))
      continue;
    /* Only capture a layer the game is actually showing on the main screen —
     * capturing a disabled layer would export nothing while still paying the
     * isolation cost, and (with RemoveFromGame) an enabled-mid-frame layer
     * would vanish from the flat fallback. */
    if (ppu->screenEnabled[0] & (1 << src))
      PpuSetOverlayCapture(ppu, (PpuOverlaySource)src, -extra, 0, frame_width,
                           frame_height, kPpuOverlayFlag_RemoveFromGame);
  }

  /* OBJ needs its OAM range declared before sprites are captured at all. */
  if ((s_profile->capture_mask & (1u << kPpuOverlaySource_Obj)) &&
      (ppu->screenEnabled[0] & (1 << kPpuOverlaySource_Obj)))
    PpuSetOverlayOamRange(ppu, 0, 128);

  for (int i = 0; i < s_profile->plane_count; i++) {
    PpuOverlaySource source;
    int band;
    int plane = s_profile->planes[i].plane;
    if (!ParallaxPlaneBand(plane, &source, &band))
      continue;
    if (!(s_profile->capture_mask & (1u << source)))
      continue;
    uint8_t *buffer = ParallaxEnsurePlaneBuffer(plane);
    if (buffer)
      PpuBindOverlayPrioSurface(ppu, source, band, buffer);
  }

  s_bound_pitch = (int)pitch;
  if (!s_active_this_frame) {
    static bool logged;
    if (!logged) {
      logged = true;
      fprintf(stderr, "[parallax] first capture: profile=%s frame=%dx%d "
                      "extra=%d mask=0x%02x mode=%d screenEnabled=0x%02x\n",
              s_profile->name ? s_profile->name : "(none)", frame_width,
              frame_height, extra, s_profile->capture_mask, PPU_mode(ppu),
              ppu->screenEnabled[0]);
      for (int src = 0; src < kPpuOverlaySource_Count; src++) {
        const PpuOverlayCapture *c = &ppu->overlayCaptures[src];
        fprintf(stderr, "[parallax]   src%d bound=%d capture=[%d,%d)x[%d,%d) "
                        "flags=0x%02x bands=%d/%d/%d\n",
                src, ppu->overlayRenderBuffer[src] != NULL, c->x0, c->x1,
                c->y0, c->y1, c->flags,
                ppu->overlayRenderBands[src][0] != NULL,
                ppu->overlayRenderBands[src][1] != NULL,
                ppu->overlayRenderBands[src][2] != NULL);
      }
    }
  }
  s_active_this_frame = true;
}

/* ── Textures ─────────────────────────────────────────────────────────── */

void Parallax_DestroyTextures(void) {
  for (int i = 0; i < kParallaxPlane_Count; i++) {
    if (s_plane_textures[i]) {
      SDL_DestroyTexture(s_plane_textures[i]);
      s_plane_textures[i] = NULL;
    }
  }
  if (s_supersample) {
    SDL_DestroyTexture(s_supersample);
    s_supersample = NULL;
  }
  s_texture_width = s_texture_height = 0;
  s_supersample_w = s_supersample_h = 0;
}

/* Every plane texture is exactly the internal frame size, so UVs are the full
 * [0,1] range. (ar-recomp instead kept kPpuBufWidth-wide textures with a
 * narrower valid sub-region, which cost it a whole class of "garbage sliver at
 * the plane edge" bugs where a sample crossed out of the written area. Sizing
 * the texture to the content removes the failure mode outright.) */
static bool ParallaxEnsureTextures(SDL_Renderer *renderer, int width,
                                   int height) {
  if (s_texture_width == width && s_texture_height == height)
    return true;
  Parallax_DestroyTextures();
  /* Driven by the PROFILE, not by which buffers happen to be allocated: a band
   * plane's buffer is allocated on its first active frame, which can land
   * after this size was first resolved, and a plane with a buffer but no
   * texture silently never draws. */
  for (int i = 0; i < s_profile->plane_count; i++) {
    int plane = s_profile->planes[i].plane;
    if (plane < 0 || plane >= kParallaxPlane_Count || s_plane_textures[plane])
      continue;
    s_plane_textures[plane] =
        SDL_CreateTexture(renderer, SDL_PIXELFORMAT_ARGB8888,
                          SDL_TEXTUREACCESS_STREAMING, width, height);
    if (!s_plane_textures[plane]) {
      fprintf(stderr, "[parallax] plane %d texture %dx%d failed: %s\n", plane,
              width, height, SDL_GetError());
      Parallax_DestroyTextures();
      return false;
    }
    SDL_SetTextureBlendMode(s_plane_textures[plane], SDL_BLENDMODE_BLEND);
  }
  s_texture_width = width;
  s_texture_height = height;
  return true;
}

/* SDL2 has no SDL_BLENDMODE_BLEND_PREMULTIPLIED (that is SDL3), but the same
 * equation is expressible with a custom blend mode. Probed once; if the
 * renderer rejects it, the smooth path stays off for the process rather than
 * drawing wrong. */
static bool ParallaxEnsurePremultiplied(SDL_Renderer *renderer) {
  if (s_premultiplied_ready) return true;
  if (s_smooth_unavailable) return false;
  s_premultiplied = SDL_ComposeCustomBlendMode(
      SDL_BLENDFACTOR_ONE, SDL_BLENDFACTOR_ONE_MINUS_SRC_ALPHA,
      SDL_BLENDOPERATION_ADD,
      SDL_BLENDFACTOR_ONE, SDL_BLENDFACTOR_ONE_MINUS_SRC_ALPHA,
      SDL_BLENDOPERATION_ADD);
  SDL_Texture *probe = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_ARGB8888,
                                          SDL_TEXTUREACCESS_TARGET, 4, 4);
  if (!probe || SDL_SetTextureBlendMode(probe, s_premultiplied) != 0) {
    fprintf(stderr, "[parallax] premultiplied edge AA unavailable (%s) — "
                    "falling back to nearest-neighbour planes\n",
            SDL_GetError());
    if (probe) SDL_DestroyTexture(probe);
    s_smooth_unavailable = true;
    return false;
  }
  SDL_DestroyTexture(probe);
  s_premultiplied_ready = true;
  return true;
}

/* Renders `source` into a shared NxN-upscaled intermediate with premultiplied
 * alpha, so the final tilted draw can sample it LINEAR without dragging
 * transparent black into the colour (the dark-fringe artifact straight-alpha
 * linear filtering produces). Compositing straight alpha onto a
 * transparent-black target with plain BLEND *is* the premultiply:
 *   dstRGB = srcRGB*srcA + 0*(1-srcA), dstA = srcA.
 * Returns NULL if unavailable; the caller then draws `source` directly. */
static SDL_Texture *ParallaxBuildSupersample(SDL_Renderer *renderer,
                                             SDL_Texture *source,
                                             int width, int height) {
  if (!g_parallax.smooth || s_smooth_unavailable) return NULL;
  if (!ParallaxEnsurePremultiplied(renderer)) return NULL;
  int want_w = width * kParallaxSupersample;
  int want_h = height * kParallaxSupersample;
  if (!s_supersample || s_supersample_w != want_w ||
      s_supersample_h != want_h) {
    if (s_supersample) SDL_DestroyTexture(s_supersample);
    s_supersample = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_ARGB8888,
                                       SDL_TEXTUREACCESS_TARGET, want_w,
                                       want_h);
    if (!s_supersample) {
      fprintf(stderr, "[parallax] supersample target %dx%d failed: %s — "
                      "smooth planes disabled\n", want_w, want_h,
              SDL_GetError());
      s_smooth_unavailable = true;
      s_supersample_w = s_supersample_h = 0;
      return NULL;
    }
    s_supersample_w = want_w;
    s_supersample_h = want_h;
    SDL_SetTextureScaleMode(s_supersample, SDL_ScaleModeLinear);
    SDL_SetTextureBlendMode(s_supersample, s_premultiplied);
  }
  /* Switching render target RESETS the viewport to the new target's full size,
   * and switching back does NOT restore the caller's — so the letterbox
   * viewport Parallax_Composite set has to be saved and re-applied around this,
   * or every plane after the first would be projected into the full window
   * instead of the letterboxed rect (invisible whenever the two happen to be
   * equal, which is exactly how this hid during bring-up). */
  SDL_Rect saved_viewport;
  SDL_RenderGetViewport(renderer, &saved_viewport);
  if (SDL_SetRenderTarget(renderer, s_supersample) != 0) {
    s_smooth_unavailable = true;
    return NULL;
  }
  SDL_SetRenderDrawBlendMode(renderer, SDL_BLENDMODE_NONE);
  SDL_SetRenderDrawColor(renderer, 0, 0, 0, 0);
  SDL_RenderClear(renderer);
  SDL_SetTextureBlendMode(source, SDL_BLENDMODE_BLEND);
  SDL_RenderCopy(renderer, source, NULL, NULL);
  SDL_SetRenderTarget(renderer, NULL);
  SDL_RenderSetViewport(renderer, &saved_viewport);
  return s_supersample;
}

void Parallax_LetterboxViewport(int out_w, int out_h, int frame_w,
                                int frame_h, bool stretch, SDL_Rect *out) {
  if (!out) return;
  if (out_w <= 0 || out_h <= 0 || frame_w <= 0 || frame_h <= 0) {
    *out = (SDL_Rect){ 0, 0, out_w > 0 ? out_w : 0, out_h > 0 ? out_h : 0 };
    return;
  }
  if (stretch) {
    *out = (SDL_Rect){ 0, 0, out_w, out_h };
    return;
  }
  /* Integer arithmetic so the rect is identical to what SDL's own logical
   * presentation would pick — a half-pixel disagreement would show up as the
   * scene shifting the instant parallax is toggled. */
  int w = out_w, h = (int)((int64_t)out_w * frame_h / frame_w);
  if (h > out_h) {
    h = out_h;
    w = (int)((int64_t)out_h * frame_w / frame_h);
  }
  *out = (SDL_Rect){ (out_w - w) / 2, (out_h - h) / 2, w, h };
}

/* ── Observability ────────────────────────────────────────────────────────
 *
 * SNESRECOMP_PARALLAX_SHOT=<path.bmp> dumps the COMPOSITED result (read back
 * from the renderer, so it is exactly what reaches the screen) once parallax
 * has been active for SNESRECOMP_PARALLAX_SHOT_FRAME frames (default 120).
 * Available in every build config — no debug server, no trace build.
 *
 * This exists because "is the effect actually rendering?" is otherwise only
 * answerable by a human looking at a window: the frame-dump hooks in
 * widescreen.c capture the PPU's residual framebuffer, which under parallax is
 * deliberately almost EMPTY (the layers were removed from it), so they cannot
 * distinguish "compositing correctly" from "drawing nothing". */
static void ParallaxWriteBmp(const char *path, const uint8_t *argb, int w,
                             int h) {
  FILE *f = fopen(path, "wb");
  if (!f) {
    fprintf(stderr, "[parallax] cannot write %s\n", path);
    return;
  }
  uint32_t img = (uint32_t)w * h * 4, off = 14 + 40, size = off + img;
  uint8_t hdr[54] = { 'B', 'M' };
  memcpy(hdr + 2, &size, 4);
  memcpy(hdr + 10, &off, 4);
  uint32_t ih = 40;
  int32_t ww = w, hh = -h;  /* negative height = top-down rows */
  uint16_t planes = 1, bpp = 32;
  memcpy(hdr + 14, &ih, 4);
  memcpy(hdr + 18, &ww, 4);
  memcpy(hdr + 22, &hh, 4);
  memcpy(hdr + 26, &planes, 2);
  memcpy(hdr + 28, &bpp, 2);
  memcpy(hdr + 34, &img, 4);
  fwrite(hdr, 1, sizeof hdr, f);
  fwrite(argb, 1, img, f);
  fclose(f);
}

/* SNESRECOMP_PARALLAX_DUMP_PLANES=<dir> writes every captured plane's raw
 * surface (plus the backdrop) as <dir>/plane_NN_<name>.bmp on the same frame
 * the screenshot fires. This answers "which layer is the missing content
 * actually in?" directly, instead of inferring it from the composited image —
 * where a plane that is empty, mis-depthed, or drawn in the wrong order all
 * look alike. */
static const char *ParallaxPlaneName(int plane) {
  switch (plane) {
    case kParallaxPlane_Bg1: return "bg1";
    case kParallaxPlane_Bg2: return "bg2";
    case kParallaxPlane_Bg3: return "bg3";
    case kParallaxPlane_Bg4: return "bg4";
    case kParallaxPlane_Obj: return "obj";
    case kParallaxPlane_Backdrop: return "backdrop";
    case kParallaxPlane_Bg1Hi: return "bg1hi";
    case kParallaxPlane_Bg2Hi: return "bg2hi";
    case kParallaxPlane_Bg3Hi: return "bg3hi";
    case kParallaxPlane_Obj1: return "obj1";
    case kParallaxPlane_Obj2: return "obj2";
    case kParallaxPlane_Obj3: return "obj3";
    default: return "unknown";
  }
}

static void ParallaxDumpPlanes(const uint8_t *backdrop, int w, int h) {
  const char *dir = getenv("SNESRECOMP_PARALLAX_DUMP_PLANES");
  if (!dir || !*dir || !s_profile) return;
  for (int i = 0; i < s_profile->plane_count; i++) {
    int plane = s_profile->planes[i].plane;
    const uint8_t *src = plane == kParallaxPlane_Backdrop
        ? backdrop : s_plane_pixels[plane];
    if (!src) continue;
    /* Count alpha and colour separately. They disagree in a load-bearing way:
     * the backdrop plane is the PPU framebuffer, which has colour but ZERO
     * alpha, so an alpha-only count reports it as empty and sends you looking
     * for a capture bug that isn't there. */
    size_t alpha = 0, colored = 0;
    for (int y = 0; y < h; y++) {
      const uint32_t *row = (const uint32_t *)(src + (size_t)y * w * 4);
      for (int x = 0; x < w; x++) {
        if (row[x] & 0xff000000u) alpha++;
        if (row[x] & 0x00ffffffu) colored++;
      }
    }
    char path[1024];
    snprintf(path, sizeof path, "%s/plane_%02d_%s.bmp", dir, plane,
             ParallaxPlaneName(plane));
    fprintf(stderr,
            "[parallax] plane %-8s z=%.2f alpha=%zu colored=%zu of %d -> %s\n",
            ParallaxPlaneName(plane), s_profile->planes[i].z, alpha, colored,
            w * h, path);
    ParallaxWriteBmp(path, src, w, h);
  }
}

static void ParallaxMaybeScreenshot(SDL_Renderer *renderer,
                                    const SDL_Rect *viewport,
                                    const uint8_t *backdrop, int frame_width,
                                    int frame_height) {
  static int checked;
  static const char *path;
  static int target_frame = 120;
  static int active_frames;
  static bool done;
  if (!checked) {
    checked = 1;
    path = getenv("SNESRECOMP_PARALLAX_SHOT");
    const char *at = getenv("SNESRECOMP_PARALLAX_SHOT_FRAME");
    if (at && *at) target_frame = atoi(at);
  }
  if (!path || done) return;
  if (++active_frames < target_frame) return;
  done = true;
  /* Same frame as the composite shot, so the planes and the picture they
   * produced can be compared directly. */
  ParallaxDumpPlanes(backdrop, frame_width, frame_height);
  int w = viewport->w, h = viewport->h;
  uint8_t *pixels = (uint8_t *)malloc((size_t)w * h * 4);
  if (!pixels) return;
  /* The viewport is still set to `viewport`, so the read rect is viewport-local
   * and (0,0,w,h) is exactly the composited scene. */
  SDL_Rect rect = { 0, 0, w, h };
  if (SDL_RenderReadPixels(renderer, &rect, SDL_PIXELFORMAT_ARGB8888, pixels,
                           w * 4) == 0) {
    ParallaxWriteBmp(path, pixels, w, h);
    fprintf(stderr, "[parallax] wrote composited frame to %s (%dx%d)\n", path,
            w, h);
  } else
    fprintf(stderr, "[parallax] RenderReadPixels failed: %s\n", SDL_GetError());
  free(pixels);
}

/* ── Mesh ─────────────────────────────────────────────────────────────── */

static void ParallaxTriangulate(int *out_indices) {
  int ii = 0, cols = PARALLAX_SUBDIV_X + 1;
  for (int row = 0; row < PARALLAX_SUBDIV_Y; row++) {
    for (int col = 0; col < PARALLAX_SUBDIV_X; col++) {
      int tl = row * cols + col;
      out_indices[ii++] = tl;
      out_indices[ii++] = tl + 1;
      out_indices[ii++] = tl + cols;
      out_indices[ii++] = tl + 1;
      out_indices[ii++] = tl + cols + 1;
      out_indices[ii++] = tl + cols;
    }
  }
}

/* Returns false if any vertex fell behind the camera plane; the caller then
 * skips the whole plane rather than drawing a folded quad. */
static bool ParallaxBuildMesh(const float mvp[16], float z_world,
                              float aspect_x, float scale, int screen_w,
                              int screen_h, SDL_Color color,
                              SDL_Vertex *out_verts) {
  int vi = 0;
  for (int row = 0; row <= PARALLAX_SUBDIV_Y; row++) {
    for (int col = 0; col <= PARALLAX_SUBDIV_X; col++) {
      float s = (float)col / PARALLAX_SUBDIV_X;
      float t = (float)row / PARALLAX_SUBDIV_Y;
      Scene3DPoint point;
      if (!Scene3D_ProjectWorldPoint(mvp, (s - 0.5f) * aspect_x * scale,
                                     (0.5f - t) * scale, z_world, screen_w,
                                     screen_h, &point))
        return false;
      out_verts[vi].position.x = point.x;
      out_verts[vi].position.y = point.y;
      out_verts[vi].tex_coord.x = s;
      out_verts[vi].tex_coord.y = t;
      out_verts[vi].color = color;
      vi++;
    }
  }
  return vi == PARALLAX_VERTS;
}

/* ── Composite ────────────────────────────────────────────────────────── */

bool Parallax_Composite(SDL_Renderer *renderer, const SDL_Rect *viewport,
                        const uint8_t *backdrop, int frame_width,
                        int frame_height) {
#if !PARALLAX_HAVE_RENDER_GEOMETRY
  static bool warned;
  if (!warned && Parallax_Enabled()) {
    warned = true;
    fprintf(stderr, "[parallax] built against SDL %d.%d.%d — needs 2.0.18+ "
                    "for SDL_RenderGeometry; presenting flat\n",
            SDL_MAJOR_VERSION, SDL_MINOR_VERSION, SDL_PATCHLEVEL);
  }
  (void)renderer; (void)viewport; (void)backdrop;
  (void)frame_width; (void)frame_height;
  return false;
#else
  if (!renderer || !viewport || !s_profile || !s_active_this_frame)
    return false;
  if (frame_width <= 0 || frame_height <= 0 ||
      frame_width > kPpuBufWidth || frame_height > kParallaxMaxHeight)
    return false;
  /* The surfaces were bound at this pitch; a mismatch means the frame size
   * changed between capture and present, so the captured rows would be read at
   * the wrong stride. Present flat for one frame instead of tearing. */
  if (s_bound_pitch != frame_width * 4)
    return false;
  if (!ParallaxEnsureTextures(renderer, frame_width, frame_height))
    return false;

  int out_w = viewport->w, out_h = viewport->h;
  if (out_w <= 0 || out_h <= 0) return false;

  /* Upload: the captured planes plus the residual framebuffer as backdrop. */
  SDL_Rect upload = { 0, 0, frame_width, frame_height };
  for (int i = 0; i < s_profile->plane_count; i++) {
    int plane = s_profile->planes[i].plane;
    const uint8_t *src = plane == kParallaxPlane_Backdrop
        ? backdrop : s_plane_pixels[plane];
    if (!src || !s_plane_textures[plane]) continue;
    SDL_UpdateTexture(s_plane_textures[plane], &upload, src, frame_width * 4);
  }

  SDL_Rect prev_viewport;
  SDL_RenderGetViewport(renderer, &prev_viewport);
  SDL_RenderSetViewport(renderer, viewport);

  /* Auto-fit distance: back the camera off far enough that the focal plane
   * fills the viewport, then add the depth of the nearest plane so a layer in
   * front of the focal plane cannot poke through the near clip. */
  float aspect_x = (float)frame_width / (float)frame_height;
  float screen_aspect = (float)out_w / (float)out_h;
  float tan_half = tanf(kParallaxFovY * 0.5f);
  float fit_h = 0.5f / tan_half;
  float fit_w = (0.5f * aspect_x) / (tan_half * screen_aspect);
  float near_z = kParallaxFocalZ;
  for (int i = 0; i < s_profile->plane_count; i++)
    if (s_profile->planes[i].z > near_z) near_z = s_profile->planes[i].z;
  float gap = (float)g_parallax.layer_gap / 100.0f;
  s_auto_distance =
      fmaxf(fit_h, fit_w) * 1.02f + (near_z - kParallaxFocalZ) * gap;

  /* The authored pose plus this frame's scroll-driven lean. The lean is added
   * here, at consume time, and never written back into g_parallax — it is
   * transient render state, not an authored setting, and persisting it would
   * slowly walk the user's camera away from where they put it. */
  Scene3DCamera cam = {
    (float)g_parallax.tilt_x_mrad / 1000.0f + s_lean_pitch,
    (float)g_parallax.tilt_y_mrad / 1000.0f + s_lean_yaw,
    g_parallax.distance_x100 > 0
        ? (float)g_parallax.distance_x100 / 100.0f : s_auto_distance,
    kParallaxFovY,
  };
  if (cam.distance < kParallaxDistMin) cam.distance = kParallaxDistMin;

  float mvp[16];
  Scene3D_BuildViewProjection(&cam, out_w, out_h, mvp);

  SDL_SetRenderDrawBlendMode(renderer, SDL_BLENDMODE_NONE);
  SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255);
  SDL_RenderFillRect(renderer, NULL);

  float shade_mix = Clampf((float)g_parallax.depth_shade / 100.0f, 0.0f, 1.0f);
  int indices[PARALLAX_INDICES];
  ParallaxTriangulate(indices);
  SDL_Vertex verts[PARALLAX_VERTS];
  SDL_Vertex shadow[PARALLAX_VERTS];
  bool drew_any = false;

  for (int i = 0; i < s_profile->plane_count; i++) {
    const ParallaxPlaneDesc *desc = &s_profile->planes[i];
    if (desc->group < kParallaxGroup_Count && !g_parallax.visible[desc->group])
      continue;
    SDL_Texture *texture = s_plane_textures[desc->plane];
    const uint8_t *src = desc->plane == kParallaxPlane_Backdrop
        ? backdrop : s_plane_pixels[desc->plane];
    if (!texture || !src) continue;
    bool is_backdrop = desc->plane == kParallaxPlane_Backdrop;

    SDL_Color shade = {
      (uint8_t)lrintf(255.0f * (1.0f + (desc->shade_r - 1.0f) * shade_mix)),
      (uint8_t)lrintf(255.0f * (1.0f + (desc->shade_g - 1.0f) * shade_mix)),
      (uint8_t)lrintf(255.0f * (1.0f + (desc->shade_b - 1.0f) * shade_mix)),
      255,
    };

    float z_world = (desc->z - kParallaxFocalZ) * gap;
    /* Depth-compensated size ("fill"). A plane pushed back subtends a SMALLER
     * screen area than the focal plane — geometrically correct, and what a
     * literal diorama looks like, but it leaves the frame's edges empty and
     * reads as a rendering fault rather than depth (the backdrop showing a
     * black border around the sky, seen during bring-up). Scaling each plane by
     * its depth ratio makes every plane subtend the same area as the focal
     * plane, so the frame stays full.
     *
     * This does NOT flatten the effect: the depth cue that matters here is
     * DIFFERENTIAL MOTION under camera movement — a near plane sweeps further
     * across the screen than a far one for the same camera yaw/pitch — plus
     * per-layer perspective keystone and depth shading. Only the static
     * size-difference cue is traded away, and that is the one that costs you
     * the whole edge of the frame.
     *
     * Planes behind the focal plane get a depth-proportional overscan on top,
     * for the corners the tilt swings past the exact-fit boundary. */
    float scale = 1.0f;
    if (g_parallax.fill) {
      float focal_depth = cam.distance;
      float plane_depth = cam.distance - z_world;
      if (focal_depth > 0.0f && plane_depth > 0.0f) {
        scale = plane_depth / focal_depth;
        if (z_world < 0.0f)
          scale *= 1.0f + kParallaxOverscanPerUnit * -z_world;
      }
    }
    if (!ParallaxBuildMesh(mvp, z_world, aspect_x, scale, out_w, out_h, shade,
                           verts))
      continue;

    /* The backdrop is the PPU's own main framebuffer, and that buffer carries
     * NO alpha — the line renderer writes 0x00RRGGBB. It is opaque by
     * definition (it is the bottom of the stack), so it draws with
     * SDL_BLENDMODE_NONE where alpha is ignored, and it must NOT go through the
     * premultiplied supersample path: premultiplying by alpha=0 multiplies the
     * whole plane to nothing. That is exactly what made the sky disappear
     * during bring-up while every captured layer (which does get 0xff alpha,
     * see PpuOverlayColor) composited fine. Skipping the smooth path here costs
     * nothing visually: a full-frame backdrop has no interior edges to
     * antialias. */
    SDL_Texture *draw = texture;
    SDL_Texture *ss = is_backdrop
        ? NULL
        : ParallaxBuildSupersample(renderer, texture, frame_width,
                                   frame_height);
    if (ss) {
      draw = ss;
    } else {
      SDL_SetTextureBlendMode(texture,
          is_backdrop ? SDL_BLENDMODE_NONE : SDL_BLENDMODE_BLEND);
    }

    if (g_parallax.shadows && !is_backdrop && desc->casts_shadow) {
      float off = (float)out_h * 0.004f;
      memcpy(shadow, verts, sizeof(shadow));
      for (int v = 0; v < PARALLAX_VERTS; v++) {
        shadow[v].position.x += off;
        shadow[v].position.y += off;
        shadow[v].color = (SDL_Color){ 0, 0, 0, 90 };
      }
      SDL_RenderGeometry(renderer, draw, shadow, PARALLAX_VERTS, indices,
                         PARALLAX_INDICES);
    }

    SDL_RenderGeometry(renderer, draw, verts, PARALLAX_VERTS, indices,
                       PARALLAX_INDICES);
    drew_any = true;
  }

  if (drew_any) {
    static bool logged;
    if (!logged) {
      logged = true;
      fprintf(stderr, "[parallax] first composite: viewport=%dx%d+%d+%d "
                      "frame=%dx%d cam(pitch=%.3f yaw=%.3f dist=%.2f)\n",
              viewport->w, viewport->h, viewport->x, viewport->y, frame_width,
              frame_height, cam.tilt_x, cam.tilt_y, cam.distance);
    }
    ParallaxMaybeScreenshot(renderer, viewport, backdrop, frame_width,
                            frame_height);
  }
  SDL_RenderSetViewport(renderer, &prev_viewport);
  return drew_any;
#endif  /* PARALLAX_HAVE_RENDER_GEOMETRY */
}
