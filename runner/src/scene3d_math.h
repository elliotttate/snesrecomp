#ifndef SNESRECOMP_SCENE3D_MATH_H
#define SNESRECOMP_SCENE3D_MATH_H

#include <stdbool.h>
#include <stdint.h>

/* Renderer- and game-agnostic 3D projection kernel for the layered-parallax
 * presenter (parallax.h). Deliberately dependency-free (no SDL, no PPU, no
 * game symbols) so it is unit-testable in isolation and reusable by any future
 * host-side 3D presentation.
 *
 * Attribution: the projection/camera model is reimplemented from
 * DerrickGold/ar-recomp's src/scene3d_math.c (an ActRaiser recompilation built
 * on snesrecomp), which introduced the pseudo-3D "diorama" presentation this
 * generalizes. See docs/PARALLAX.md and IMPROVEMENTS.md.
 */

typedef struct Scene3DCamera {
  float tilt_x;    /* pitch, radians */
  float tilt_y;    /* yaw, radians */
  float distance;  /* world units back from the origin */
  float fov_y;     /* vertical field of view, radians */
} Scene3DCamera;

typedef struct Scene3DPoint {
  float x, y;
} Scene3DPoint;

/* Column-major 4x4 view-projection for `camera` at the given output size. */
void Scene3D_BuildViewProjection(const Scene3DCamera *camera,
                                 int output_width, int output_height,
                                 float out_matrix[16]);

/* Projects a world point only while it is safely in front of the camera
 * plane. Returns false for points on/behind that plane or for non-finite
 * results, and leaves `out_point` untouched on failure. Callers that build a
 * primitive must reject the whole primitive if any of its vertices fail —
 * a partially-projected quad folds inside out. */
bool Scene3D_ProjectWorldPoint(const float matrix[16],
                               float x, float y, float z,
                               int output_width, int output_height,
                               Scene3DPoint *out_point);

/* Camera distance at which a unit-height quad exactly fills the vertical
 * field of view. */
float Scene3D_AutoFitDistance(float fov_y);

#endif  /* SNESRECOMP_SCENE3D_MATH_H */
