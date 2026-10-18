/*
 * shaders.h - minimal shader program management for the VHMS UI.
 *
 * A single quad program serves both flat-color rectangles and textured text
 * quads: the u_use_tex uniform selects between "alpha = 1" (rects) and
 * "alpha = texture alpha" (glyph quads).  One program keeps the pipeline
 * state - and the certification evidence - as small as possible.
 */
#ifndef VHMS_SHADERS_H
#define VHMS_SHADERS_H

#include "gl_portability.h"

typedef struct {
    GLuint program;
    GLint  a_pos;      /* vec2, pixels */
    GLint  a_uv;       /* vec2, font-atlas UV */
    GLint  a_color;    /* vec4, per-vertex RGBA */
    GLint  u_proj;     /* mat4 orthographic projection */
    GLint  u_tex;      /* sampler2D font atlas */
    GLint  u_use_tex;  /* float: 0 = flat color, 1 = textured */
} vhms_shaders_t;

/* Compile, link, bind attribute locations, resolve uniforms.
 * Returns 0 on success, -1 on failure (diagnostics go to stderr). */
int vhms_shaders_init(vhms_shaders_t *sh);
void vhms_shaders_shutdown(vhms_shaders_t *sh);

#endif /* VHMS_SHADERS_H */
