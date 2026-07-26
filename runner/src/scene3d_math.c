#include "scene3d_math.h"

#include <math.h>

/* Reimplemented from DerrickGold/ar-recomp src/scene3d_math.c — see the header
 * comment for attribution. */

static const float kMinimumProjectionDepth = 0.0001f;

static void Mat4Mul(const float a[16], const float b[16], float out[16]) {
  for (int column = 0; column < 4; column++) {
    for (int row = 0; row < 4; row++) {
      float sum = 0.0f;
      for (int k = 0; k < 4; k++)
        sum += a[k * 4 + row] * b[column * 4 + k];
      out[column * 4 + row] = sum;
    }
  }
}

void Scene3D_BuildViewProjection(const Scene3DCamera *camera,
                                 int output_width, int output_height,
                                 float out_matrix[16]) {
  const float near_plane = 0.1f, far_plane = 100.0f;
  float aspect = output_height > 0
      ? (float)output_width / (float)output_height : 1.0f;
  float f = 1.0f / tanf(camera->fov_y * 0.5f);
  float projection[16] = {
    f / aspect, 0, 0, 0,
    0, f, 0, 0,
    0, 0, (far_plane + near_plane) / (near_plane - far_plane), -1,
    0, 0, (2 * far_plane * near_plane) / (near_plane - far_plane), 0,
  };
  float cx = cosf(camera->tilt_x), sx = sinf(camera->tilt_x);
  float cy = cosf(camera->tilt_y), sy = sinf(camera->tilt_y);
  float rotate_y[16] = {
    cy,0,sy,0,  0,1,0,0,  -sy,0,cy,0,  0,0,0,1,
  };
  float rotate_x[16] = {
    1,0,0,0,  0,cx,sx,0,  0,-sx,cx,0,  0,0,0,1,
  };
  float translate[16] = {
    1,0,0,0,  0,1,0,0,  0,0,1,0,  0,0,-camera->distance,1,
  };
  float rotation[16], view[16];
  Mat4Mul(rotate_x, rotate_y, rotation);
  Mat4Mul(translate, rotation, view);
  Mat4Mul(projection, view, out_matrix);
}

bool Scene3D_ProjectWorldPoint(const float matrix[16],
                               float x, float y, float z,
                               int output_width, int output_height,
                               Scene3DPoint *out_point) {
  if (!out_point) return false;
  float clip_x = matrix[0] * x + matrix[4] * y + matrix[8] * z + matrix[12];
  float clip_y = matrix[1] * x + matrix[5] * y + matrix[9] * z + matrix[13];
  float clip_w = matrix[3] * x + matrix[7] * y + matrix[11] * z + matrix[15];
  if (clip_w <= kMinimumProjectionDepth) return false;
  float inverse_w = 1.0f / clip_w;
  Scene3DPoint projected = {
    (clip_x * inverse_w * 0.5f + 0.5f) * (float)output_width,
    (1.0f - (clip_y * inverse_w * 0.5f + 0.5f)) * (float)output_height,
  };
  if (!isfinite(projected.x) || !isfinite(projected.y)) return false;
  *out_point = projected;
  return true;
}

float Scene3D_AutoFitDistance(float fov_y) {
  return 0.5f / tanf(fov_y * 0.5f);
}
