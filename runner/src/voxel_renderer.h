/*
 * voxel_renderer.h -- optional presentation-only SNES screen-grid compositor
 *
 * A game describes a rectangular part of its final ARGB8888 frame as a grid
 * of square cells and supplies a height policy.  The compositor projects those
 * live pixels onto raised prisms.  It never mutates emulated CPU, PPU, input,
 * timing, or save-state data.
 */
#ifndef SNESRECOMP_VOXEL_RENDERER_H
#define SNESRECOMP_VOXEL_RENDERER_H

#include <stdint.h>

typedef float (*SnesVoxelHeightFn)(const uint32_t *cell_pixels,
                                   int pixel_stride,
                                   int cell_size,
                                   int cell_x,
                                   int cell_y,
                                   void *user);

typedef struct SnesVoxelBillboard {
  const uint32_t *pixels;
  int pixel_stride;
  int texture_width;
  int texture_height;
  float world_x;
  float world_z;
  float base_height;
  float world_width;
  float world_height;
} SnesVoxelBillboard;

typedef struct SnesVoxelScene {
  uint32_t *framebuffer;
  int framebuffer_stride;
  int output_width;
  int output_height;

  /* Native-frame rectangle represented by the voxel grid. */
  int source_x;
  int source_y;
  int source_width;
  int source_height;
  int cell_size;
  SnesVoxelHeightFn cell_height;
  void *user;

  /* Optional upright, camera-facing sprites composited after terrain. */
  const SnesVoxelBillboard *billboards;
  int billboard_count;

  /* Orbit camera.  Positive elevation looks down at the scene. */
  float elevation_degrees;
  float yaw_degrees;
  float roll_degrees;
  float camera_distance;
  float camera_focal_scale;
  float camera_center_y;

  /* Optional explicit camera pose, used by first-person experiments. */
  int use_camera_pose;
  float camera_eye_x;
  float camera_eye_y;
  float camera_eye_z;
  float camera_look_at_x;
  float camera_look_at_y;
  float camera_look_at_z;

  /* Rows copied back from the native frame after 3D composition (HUD). */
  int preserve_top_rows;

  uint32_t sky_top;
  uint32_t sky_bottom;
} SnesVoxelScene;

/* Returns 1 when a scene was rendered, or 0 for an invalid descriptor. */
int snes_voxel_render(const SnesVoxelScene *scene);

#endif
