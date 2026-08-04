/*
 * voxel_renderer.c -- compact software 3D compositor for SNESRecomp
 *
 * The final host frame is the texture atlas.  Keeping this after the emulated
 * PPU makes the experiment renderer-backend independent and guarantees that
 * disabling it restores the authentic frame without changing game state.
 */
#include "voxel_renderer.h"

#include <math.h>
#include <stddef.h>
#include <string.h>

#define VOXEL_MAX_WIDTH 512
#define VOXEL_MAX_HEIGHT 240
#define VOXEL_MAX_COLUMNS 64
#define VOXEL_MAX_ROWS 30
#define VOXEL_PI 3.14159265358979323846f

typedef struct Vec3 {
  float x, y, z;
} Vec3;

typedef struct ProjectedVertex {
  float x, y;
  float inv_depth;
  float u, v;
} ProjectedVertex;

typedef struct Texture {
  const uint32_t *pixels;
  int width, height, stride;
  float shade;
} Texture;

typedef struct RenderContext {
  const SnesVoxelScene *scene;
  Vec3 eye, right, up, forward;
  float focal;
  float center_x, center_y;
  int columns, rows;
} RenderContext;

static uint32_t s_source[VOXEL_MAX_WIDTH * VOXEL_MAX_HEIGHT];
static float s_depth[VOXEL_MAX_WIDTH * VOXEL_MAX_HEIGHT];
static float s_heights[VOXEL_MAX_COLUMNS * VOXEL_MAX_ROWS];

static Vec3 vec3(float x, float y, float z) {
  Vec3 v = {x, y, z};
  return v;
}

static Vec3 vec3_sub(Vec3 a, Vec3 b) {
  return vec3(a.x - b.x, a.y - b.y, a.z - b.z);
}

static Vec3 vec3_scale(Vec3 v, float scale) {
  return vec3(v.x * scale, v.y * scale, v.z * scale);
}

static float vec3_dot(Vec3 a, Vec3 b) {
  return a.x * b.x + a.y * b.y + a.z * b.z;
}

static Vec3 vec3_cross(Vec3 a, Vec3 b) {
  return vec3(a.y * b.z - a.z * b.y,
              a.z * b.x - a.x * b.z,
              a.x * b.y - a.y * b.x);
}

static Vec3 vec3_normalize(Vec3 v) {
  float length = sqrtf(vec3_dot(v, v));
  return length > 0.00001f ? vec3_scale(v, 1.0f / length) : v;
}

static uint32_t shade_color(uint32_t color, float shade) {
  unsigned alpha = color >> 24;
  unsigned red = (color >> 16) & 0xff;
  unsigned green = (color >> 8) & 0xff;
  unsigned blue = color & 0xff;
  red = (unsigned)(red * shade);
  green = (unsigned)(green * shade);
  blue = (unsigned)(blue * shade);
  if (red > 255) red = 255;
  if (green > 255) green = 255;
  if (blue > 255) blue = 255;
  return (alpha << 24) | (red << 16) | (green << 8) | blue;
}

static uint32_t lerp_color(uint32_t a, uint32_t b, float t) {
  int ar = (a >> 16) & 0xff, ag = (a >> 8) & 0xff, ab = a & 0xff;
  int br = (b >> 16) & 0xff, bg = (b >> 8) & 0xff, bb = b & 0xff;
  unsigned red = (unsigned)(ar + (br - ar) * t);
  unsigned green = (unsigned)(ag + (bg - ag) * t);
  unsigned blue = (unsigned)(ab + (bb - ab) * t);
  return 0xff000000u | (red << 16) | (green << 8) | blue;
}

static float edge(float ax, float ay, float bx, float by,
                  float px, float py) {
  return (px - ax) * (by - ay) - (py - ay) * (bx - ax);
}

static int project_vertex(const RenderContext *ctx, Vec3 world,
                          float u, float v, ProjectedVertex *out) {
  Vec3 relative = vec3_sub(world, ctx->eye);
  float depth = vec3_dot(relative, ctx->forward);
  if (depth < 0.75f)
    return 0;
  out->inv_depth = 1.0f / depth;
  out->x = ctx->center_x +
           vec3_dot(relative, ctx->right) * ctx->focal / depth;
  out->y = ctx->center_y -
           vec3_dot(relative, ctx->up) * ctx->focal / depth;
  out->u = u;
  out->v = v;
  return 1;
}

static void draw_triangle(const RenderContext *ctx,
                          const ProjectedVertex *a,
                          const ProjectedVertex *b,
                          const ProjectedVertex *c,
                          const Texture *texture) {
  const SnesVoxelScene *scene = ctx->scene;
  float area = edge(a->x, a->y, b->x, b->y, c->x, c->y);
  int min_x, max_x, min_y, max_y;
  if (fabsf(area) < 0.0001f)
    return;

  min_x = (int)floorf(fminf(a->x, fminf(b->x, c->x)));
  max_x = (int)ceilf(fmaxf(a->x, fmaxf(b->x, c->x)));
  min_y = (int)floorf(fminf(a->y, fminf(b->y, c->y)));
  max_y = (int)ceilf(fmaxf(a->y, fmaxf(b->y, c->y)));
  if (min_x < 0) min_x = 0;
  if (min_y < 0) min_y = 0;
  if (max_x >= scene->output_width) max_x = scene->output_width - 1;
  if (max_y >= scene->output_height) max_y = scene->output_height - 1;

  for (int y = min_y; y <= max_y; y++) {
    for (int x = min_x; x <= max_x; x++) {
      float px = x + 0.5f, py = y + 0.5f;
      float w0 = edge(b->x, b->y, c->x, c->y, px, py) / area;
      float w1 = edge(c->x, c->y, a->x, a->y, px, py) / area;
      float w2 = 1.0f - w0 - w1;
      float inv_depth, u, v;
      int texture_x, texture_y, depth_pos, frame_pos;
      uint32_t color;
      if (w0 < -0.0001f || w1 < -0.0001f || w2 < -0.0001f)
        continue;

      inv_depth = w0 * a->inv_depth + w1 * b->inv_depth +
                  w2 * c->inv_depth;
      depth_pos = y * scene->output_width + x;
      if (inv_depth <= s_depth[depth_pos])
        continue;

      u = (w0 * a->u * a->inv_depth +
           w1 * b->u * b->inv_depth +
           w2 * c->u * c->inv_depth) / inv_depth;
      v = (w0 * a->v * a->inv_depth +
           w1 * b->v * b->inv_depth +
           w2 * c->v * c->inv_depth) / inv_depth;
      texture_x = (int)floorf(u);
      texture_y = (int)floorf(v);
      if (texture_x < 0) texture_x = 0;
      if (texture_y < 0) texture_y = 0;
      if (texture_x >= texture->width) texture_x = texture->width - 1;
      if (texture_y >= texture->height) texture_y = texture->height - 1;
      color = texture->pixels[texture_y * texture->stride + texture_x];
      frame_pos = y * scene->framebuffer_stride + x;
      scene->framebuffer[frame_pos] = shade_color(color, texture->shade);
      s_depth[depth_pos] = inv_depth;
    }
  }
}

static void draw_quad(const RenderContext *ctx,
                      Vec3 a, Vec3 b, Vec3 c, Vec3 d,
                      const Texture *texture) {
  ProjectedVertex pa, pb, pc, pd;
  float max_u = (float)texture->width - 0.001f;
  float max_v = (float)texture->height - 0.001f;
  if (!project_vertex(ctx, a, 0.0f, 0.0f, &pa) ||
      !project_vertex(ctx, b, max_u, 0.0f, &pb) ||
      !project_vertex(ctx, c, max_u, max_v, &pc) ||
      !project_vertex(ctx, d, 0.0f, max_v, &pd))
    return;
  draw_triangle(ctx, &pa, &pb, &pc, texture);
  draw_triangle(ctx, &pa, &pc, &pd, texture);
}

static float cell_height(const RenderContext *ctx, int x, int y) {
  if (x < 0 || y < 0 || x >= ctx->columns || y >= ctx->rows)
    return -8.0f;
  return s_heights[y * ctx->columns + x];
}

static Texture cell_texture(const RenderContext *ctx, int x, int y,
                            float shade) {
  const SnesVoxelScene *scene = ctx->scene;
  Texture texture;
  int source_x = scene->source_x + x * scene->cell_size;
  int source_y = scene->source_y + y * scene->cell_size;
  texture.pixels = s_source + source_y * scene->output_width + source_x;
  texture.width = scene->cell_size;
  texture.height = scene->cell_size;
  texture.stride = scene->output_width;
  texture.shade = shade;
  return texture;
}

static void render_terrain(const RenderContext *ctx) {
  const SnesVoxelScene *scene = ctx->scene;
  float size = (float)scene->cell_size;
  for (int y = 0; y < ctx->rows; y++) {
    for (int x = 0; x < ctx->columns; x++) {
      float x0 = x * size, x1 = x0 + size;
      float z0 = y * size, z1 = z0 + size;
      float height = cell_height(ctx, x, y);
      float north = cell_height(ctx, x, y - 1);
      float south = cell_height(ctx, x, y + 1);
      float west = cell_height(ctx, x - 1, y);
      float east = cell_height(ctx, x + 1, y);
      Texture top = cell_texture(ctx, x, y, height < 0.0f ? 0.84f : 1.0f);
      Texture side_dark = cell_texture(ctx, x, y, 0.55f);
      Texture side_light = cell_texture(ctx, x, y, 0.72f);

      draw_quad(ctx, vec3(x0, height, z0), vec3(x1, height, z0),
                vec3(x1, height, z1), vec3(x0, height, z1), &top);
      if (height > north)
        draw_quad(ctx, vec3(x1, height, z0), vec3(x0, height, z0),
                  vec3(x0, north, z0), vec3(x1, north, z0), &side_dark);
      if (height > south)
        draw_quad(ctx, vec3(x0, height, z1), vec3(x1, height, z1),
                  vec3(x1, south, z1), vec3(x0, south, z1), &side_light);
      if (height > west)
        draw_quad(ctx, vec3(x0, height, z0), vec3(x0, height, z1),
                  vec3(x0, west, z1), vec3(x0, west, z0), &side_dark);
      if (height > east)
        draw_quad(ctx, vec3(x1, height, z1), vec3(x1, height, z0),
                  vec3(x1, east, z0), vec3(x1, east, z1), &side_light);
    }
  }
}

static int validate_scene(const SnesVoxelScene *scene,
                          int *columns, int *rows) {
  if (!scene || !scene->framebuffer || !scene->cell_height)
    return 0;
  if (scene->output_width <= 0 || scene->output_width > VOXEL_MAX_WIDTH ||
      scene->output_height <= 0 || scene->output_height > VOXEL_MAX_HEIGHT ||
      scene->framebuffer_stride < scene->output_width)
    return 0;
  if (scene->cell_size <= 0 ||
      scene->source_width <= 0 || scene->source_height <= 0 ||
      scene->source_width % scene->cell_size != 0 ||
      scene->source_height % scene->cell_size != 0)
    return 0;
  *columns = scene->source_width / scene->cell_size;
  *rows = scene->source_height / scene->cell_size;
  if (*columns <= 0 || *columns > VOXEL_MAX_COLUMNS ||
      *rows <= 0 || *rows > VOXEL_MAX_ROWS)
    return 0;
  if (scene->source_x < 0 || scene->source_y < 0 ||
      scene->source_x + scene->source_width > scene->output_width ||
      scene->source_y + scene->source_height > scene->output_height)
    return 0;
  return 1;
}

int snes_voxel_render(const SnesVoxelScene *scene) {
  RenderContext ctx;
  Vec3 target;
  float elevation, yaw, roll, distance, horizontal_distance;
  uint32_t sky_top, sky_bottom;
  int columns, rows;

  if (!validate_scene(scene, &columns, &rows))
    return 0;

  for (int y = 0; y < scene->output_height; y++) {
    memcpy(s_source + y * scene->output_width,
           scene->framebuffer + y * scene->framebuffer_stride,
           (size_t)scene->output_width * sizeof(uint32_t));
  }
  memset(s_depth, 0,
         (size_t)scene->output_width * scene->output_height * sizeof(float));

  for (int y = 0; y < rows; y++) {
    for (int x = 0; x < columns; x++) {
      const uint32_t *pixels =
          s_source + (scene->source_y + y * scene->cell_size) *
                         scene->output_width +
                     scene->source_x + x * scene->cell_size;
      s_heights[y * columns + x] =
          scene->cell_height(pixels, scene->output_width, scene->cell_size,
                             x, y, scene->user);
    }
  }

  sky_top = scene->sky_top ? scene->sky_top : 0xff18243au;
  sky_bottom = scene->sky_bottom ? scene->sky_bottom : 0xff6688a0u;
  for (int y = 0; y < scene->output_height; y++) {
    float t = (float)y / (float)(scene->output_height - 1);
    uint32_t color = lerp_color(sky_top, sky_bottom, t);
    for (int x = 0; x < scene->output_width; x++)
      scene->framebuffer[y * scene->framebuffer_stride + x] = color;
  }

  ctx.scene = scene;
  ctx.columns = columns;
  ctx.rows = rows;
  if (scene->use_camera_pose) {
    ctx.eye = vec3(scene->camera_eye_x, scene->camera_eye_y,
                   scene->camera_eye_z);
    target = vec3(scene->camera_look_at_x, scene->camera_look_at_y,
                  scene->camera_look_at_z);
  } else {
    target = vec3(scene->source_width * 0.5f, 2.0f,
                  scene->source_height * 0.5f);
    elevation = scene->elevation_degrees * VOXEL_PI / 180.0f;
    yaw = scene->yaw_degrees * VOXEL_PI / 180.0f;
    distance = scene->camera_distance > 1.0f
                   ? scene->camera_distance
                   : 285.0f;
    horizontal_distance = cosf(elevation) * distance;
    ctx.eye = vec3(target.x + sinf(yaw) * horizontal_distance,
                   target.y + sinf(elevation) * distance,
                   target.z + cosf(yaw) * horizontal_distance);
  }

  ctx.forward = vec3_normalize(vec3_sub(target, ctx.eye));
  ctx.right =
      vec3_normalize(vec3_cross(ctx.forward, vec3(0.0f, 1.0f, 0.0f)));
  ctx.up = vec3_normalize(vec3_cross(ctx.right, ctx.forward));
  roll = scene->roll_degrees * VOXEL_PI / 180.0f;
  if (fabsf(roll) > 0.0001f) {
    Vec3 base_right = ctx.right;
    Vec3 base_up = ctx.up;
    ctx.right = vec3_normalize(vec3(
        base_right.x * cosf(roll) + base_up.x * sinf(roll),
        base_right.y * cosf(roll) + base_up.y * sinf(roll),
        base_right.z * cosf(roll) + base_up.z * sinf(roll)));
    ctx.up = vec3_normalize(vec3(
        base_up.x * cosf(roll) - base_right.x * sinf(roll),
        base_up.y * cosf(roll) - base_right.y * sinf(roll),
        base_up.z * cosf(roll) - base_right.z * sinf(roll)));
  }
  ctx.focal = scene->output_width *
              (scene->camera_focal_scale > 0.05f
                   ? scene->camera_focal_scale
                   : 0.92f);
  ctx.center_x = scene->output_width * 0.5f;
  ctx.center_y = scene->output_height *
                 (scene->camera_center_y > 0.05f &&
                          scene->camera_center_y < 0.95f
                      ? scene->camera_center_y
                      : 0.59f);

  render_terrain(&ctx);

  if (scene->preserve_top_rows > 0) {
    int rows_to_copy = scene->preserve_top_rows;
    if (rows_to_copy > scene->output_height)
      rows_to_copy = scene->output_height;
    for (int y = 0; y < rows_to_copy; y++) {
      memcpy(scene->framebuffer + y * scene->framebuffer_stride,
             s_source + y * scene->output_width,
             (size_t)scene->output_width * sizeof(uint32_t));
    }
  }
  return 1;
}
