/*
 * ui_core.c - static-buffer 2D renderer.
 *
 * Rect batch vertex:  x, y, r, g, b, a            (6 floats)
 * Text batch vertex:  x, y, u, v, r, g, b, a      (8 floats)
 * Quads are emitted as two triangles (6 vertices); no index buffer, no
 * triangle strips, so there are no degenerate-triangle edge cases.
 */
#include "ui_core.h"
#include "font_8x8.h"

#include <assert.h>
#include <string.h>

#define ATLAS_CELLS 16
#define ATLAS_PX    128

#define FLAT_FLOATS 6u
#define TEXT_FLOATS 8u

static float s_flat_verts[UI_MAX_RECTS * 6u * FLAT_FLOATS];
static float s_text_verts[UI_MAX_GLYPHS * 6u * TEXT_FLOATS];
static unsigned s_flat_quads = 0;
static unsigned s_text_quads = 0;

static GLuint s_vbo_flat = 0;
static GLuint s_vbo_text = 0;
static GLuint s_font_tex = 0;

static vhms_shaders_t s_sh;
static float s_proj[16];

/* RGBA8 font atlas, white texels keyed by alpha.  Stored bottom-up because
 * glTexImage2D row 0 maps to v = 0. */
static unsigned char s_atlas[ATLAS_PX * ATLAS_PX * 4u];

static void ui_build_atlas(void)
{
    unsigned g, r, c;

    memset(s_atlas, 0, sizeof(s_atlas));
    for (g = 0; g < (unsigned)VHMS_FONT_COUNT; g++) {
        unsigned gx = g % ATLAS_CELLS;
        unsigned gy = g / ATLAS_CELLS;
        for (r = 0; r < 8u; r++) {
            uint8_t bits = vhms_font_8x8[g][r];
            for (c = 0; c < 8u; c++) {
                unsigned on   = (unsigned)((bits >> (7u - c)) & 1u);
                unsigned drow = (ATLAS_PX - 1u) - (gy * 8u + r);
                unsigned dcol = gx * 8u + c;
                unsigned char *p = &s_atlas[(drow * ATLAS_PX + dcol) * 4u];
                p[0] = 255;
                p[1] = 255;
                p[2] = 255;
                p[3] = on ? 255u : 0u;
            }
        }
    }
}

int ui_init(void)
{
    if (vhms_shaders_init(&s_sh) != 0)
        return -1;

    glGenBuffers(1, &s_vbo_flat);
    glGenBuffers(1, &s_vbo_text);

    ui_build_atlas();
    glGenTextures(1, &s_font_tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, s_font_tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, ATLAS_PX, ATLAS_PX, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, s_atlas);

    /* Blending stays on for the life of the program; opaque rects carry
     * alpha 1.0 so the blend is a no-op for them. */
    glEnable(GL_BLEND);
    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);

    return 0;
}

void ui_shutdown(void)
{
    if (s_vbo_flat) {
        glDeleteBuffers(1, &s_vbo_flat);
        s_vbo_flat = 0;
    }
    if (s_vbo_text) {
        glDeleteBuffers(1, &s_vbo_text);
        s_vbo_text = 0;
    }
    if (s_font_tex) {
        glDeleteTextures(1, &s_font_tex);
        s_font_tex = 0;
    }
    vhms_shaders_shutdown(&s_sh);
}

void ui_begin_frame(int width, int height)
{
    assert(width > 0 && height > 0);

    glViewport(0, 0, width, height);
    glClearColor(0.043f, 0.055f, 0.078f, 1.0f); /* #0b0e14 cockpit background */
    glClear(GL_COLOR_BUFFER_BIT);

    /* Orthographic projection, origin top-left, units in pixels. */
    memset(s_proj, 0, sizeof(s_proj));
    s_proj[0]  = 2.0f / (float)width;
    s_proj[5]  = -2.0f / (float)height;
    s_proj[10] = -1.0f;
    s_proj[12] = -1.0f;
    s_proj[13] = 1.0f;
    s_proj[15] = 1.0f;

    s_flat_quads = 0;
    s_text_quads = 0;
}

static void ui_emit_flat_vert(float *dst, float x, float y, ui_color_t c)
{
    dst[0] = x;
    dst[1] = y;
    dst[2] = c.r;
    dst[3] = c.g;
    dst[4] = c.b;
    dst[5] = c.a;
}

void ui_draw_rect(float x, float y, float w, float h, ui_color_t color)
{
    float *v;
    float x1, y1;

    assert(s_flat_quads < UI_MAX_RECTS);
    if (s_flat_quads >= UI_MAX_RECTS)
        return;
    if (w <= 0.0f || h <= 0.0f)
        return;

    x1 = x + w;
    y1 = y + h;
    v = &s_flat_verts[s_flat_quads * 6u * FLAT_FLOATS];
    ui_emit_flat_vert(v + 0 * FLAT_FLOATS, x, y, color);
    ui_emit_flat_vert(v + 1 * FLAT_FLOATS, x, y1, color);
    ui_emit_flat_vert(v + 2 * FLAT_FLOATS, x1, y1, color);
    ui_emit_flat_vert(v + 3 * FLAT_FLOATS, x, y, color);
    ui_emit_flat_vert(v + 4 * FLAT_FLOATS, x1, y1, color);
    ui_emit_flat_vert(v + 5 * FLAT_FLOATS, x1, y, color);
    s_flat_quads++;
}

static void ui_emit_glyph(float x, float y, float scale, ui_color_t color,
                          unsigned glyph)
{
    float *v;
    float x1 = x + 8.0f * scale;
    float y1 = y + 8.0f * scale;
    /* Atlas is stored bottom-up: glyph row 0 (the top row) lives in the
     * highest data rows, i.e. near v = 1.  Data row d covers
     * v in [d/128, (d+1)/128), and glyph (gx,gy) row r sits at data row
     * 127 - (gy*8 + r).  Hence the glyph's top edge is at v = 1 - gy*8/128
     * and its bottom edge at v = 1 - (gy*8+8)/128. */
    float gx = (float)(glyph % ATLAS_CELLS);
    float gy = (float)(glyph / ATLAS_CELLS);
    float s0 = (gx * 8.0f) / (float)ATLAS_PX;
    float s1 = (gx * 8.0f + 8.0f) / (float)ATLAS_PX;
    float t_top = 1.0f - (gy * 8.0f) / (float)ATLAS_PX;
    float t_bot = 1.0f - (gy * 8.0f + 8.0f) / (float)ATLAS_PX;
    unsigned i;
    static const float corner[6][4] = {
        /* x, y, u, v per triangle vertex */
        { 0.0f, 0.0f, 0.0f, 1.0f }, /* TL */
        { 0.0f, 1.0f, 0.0f, 0.0f }, /* BL */
        { 1.0f, 1.0f, 1.0f, 0.0f }, /* BR */
        { 0.0f, 0.0f, 0.0f, 1.0f }, /* TL */
        { 1.0f, 1.0f, 1.0f, 0.0f }, /* BR */
        { 1.0f, 0.0f, 1.0f, 1.0f }, /* TR */
    };

    assert(s_text_quads < UI_MAX_GLYPHS);
    if (s_text_quads >= UI_MAX_GLYPHS)
        return;

    v = &s_text_verts[s_text_quads * 6u * TEXT_FLOATS];
    for (i = 0; i < 6u; i++) {
        float *d = v + i * TEXT_FLOATS;
        d[0] = x + corner[i][0] * (x1 - x);
        d[1] = y + corner[i][1] * (y1 - y);
        d[2] = s0 + corner[i][2] * (s1 - s0);
        d[3] = t_bot + corner[i][3] * (t_top - t_bot);
        d[4] = color.r;
        d[5] = color.g;
        d[6] = color.b;
        d[7] = color.a;
    }
    s_text_quads++;
}

void ui_draw_text(float x, float y, float scale, ui_color_t color,
                 const char *str)
{
    float cx = x;
    const char *p;

    if (!str || scale <= 0.0f)
        return;
    for (p = str; *p; p++) {
        unsigned ch = (unsigned char)*p;
        unsigned glyph;
        if (ch < 32u || ch > 126u)
            ch = (unsigned)'?';
        glyph = ch - 32u;
        ui_emit_glyph(cx, y, scale, color, glyph);
        cx += 8.0f * scale;
    }
}

float ui_text_width(const char *str, float scale)
{
    if (!str)
        return 0.0f;
    return (float)strlen(str) * 8.0f * scale;
}

void ui_draw_text_bounded(float x, float y, float scale, ui_color_t color,
                          const char *str, float max_width_px)
{
    /* Longest string this UI draws is bounded by the data model (< 256
     * chars); 512 bytes of stack leaves comfortable headroom. */
    char tmp[512];
    size_t n, max_chars, keep;

    if (!str || scale <= 0.0f || max_width_px <= 0.0f)
        return;

    n = strlen(str);
    assert(n < sizeof(tmp) - 4u);
    if (n >= sizeof(tmp) - 4u)
        n = sizeof(tmp) - 5u;

    max_chars = (size_t)(max_width_px / (8.0f * scale));
    if (max_chars == 0u)
        return;
    if (n <= max_chars) {
        ui_draw_text(x, y, scale, color, str);
        return;
    }
    if (max_chars <= 3u) {
        size_t i;
        for (i = 0; i < max_chars; i++)
            tmp[i] = '.';
        tmp[max_chars] = '\0';
        ui_draw_text(x, y, scale, color, tmp);
        return;
    }
    keep = max_chars - 3u;
    memcpy(tmp, str, keep);
    tmp[keep] = '.';
    tmp[keep + 1u] = '.';
    tmp[keep + 2u] = '.';
    tmp[keep + 3u] = '\0';
    ui_draw_text(x, y, scale, color, tmp);
}

void ui_end_frame(void)
{
    GLsizeiptr flat_bytes;
    GLsizeiptr text_bytes;

    glUseProgram(s_sh.program);
    glUniformMatrix4fv(s_sh.u_proj, 1, GL_FALSE, s_proj);

    /* Pass 1: flat-color rects. */
    glUniform1f(s_sh.u_use_tex, 0.0f);
    glBindBuffer(GL_ARRAY_BUFFER, s_vbo_flat);
    flat_bytes = (GLsizeiptr)(s_flat_quads * 6u * FLAT_FLOATS * sizeof(float));
    if (flat_bytes > 0) {
        glBufferData(GL_ARRAY_BUFFER, flat_bytes, s_flat_verts,
                     GL_DYNAMIC_DRAW);
        glEnableVertexAttribArray((GLuint)s_sh.a_pos);
        glVertexAttribPointer((GLuint)s_sh.a_pos, 2, GL_FLOAT, GL_FALSE,
                             FLAT_FLOATS * (GLsizei)sizeof(float),
                             (const void *)0);
        glEnableVertexAttribArray((GLuint)s_sh.a_color);
        glVertexAttribPointer((GLuint)s_sh.a_color, 4, GL_FLOAT, GL_FALSE,
                             FLAT_FLOATS * (GLsizei)sizeof(float),
                             (const void *)(2u * sizeof(float)));
        glDisableVertexAttribArray((GLuint)s_sh.a_uv);
        glVertexAttrib2f((GLuint)s_sh.a_uv, 0.0f, 0.0f);
        glDrawArrays(GL_TRIANGLES, 0, (GLsizei)(s_flat_quads * 6u));
    }

    /* Pass 2: textured glyph quads. */
    glUniform1f(s_sh.u_use_tex, 1.0f);
    glBindBuffer(GL_ARRAY_BUFFER, s_vbo_text);
    text_bytes = (GLsizeiptr)(s_text_quads * 6u * TEXT_FLOATS * sizeof(float));
    if (text_bytes > 0) {
        glBufferData(GL_ARRAY_BUFFER, text_bytes, s_text_verts,
                     GL_DYNAMIC_DRAW);
        glEnableVertexAttribArray((GLuint)s_sh.a_pos);
        glVertexAttribPointer((GLuint)s_sh.a_pos, 2, GL_FLOAT, GL_FALSE,
                             TEXT_FLOATS * (GLsizei)sizeof(float),
                             (const void *)0);
        glEnableVertexAttribArray((GLuint)s_sh.a_uv);
        glVertexAttribPointer((GLuint)s_sh.a_uv, 2, GL_FLOAT, GL_FALSE,
                             TEXT_FLOATS * (GLsizei)sizeof(float),
                             (const void *)(2u * sizeof(float)));
        glEnableVertexAttribArray((GLuint)s_sh.a_color);
        glVertexAttribPointer((GLuint)s_sh.a_color, 4, GL_FLOAT, GL_FALSE,
                             TEXT_FLOATS * (GLsizei)sizeof(float),
                             (const void *)(4u * sizeof(float)));
        glDrawArrays(GL_TRIANGLES, 0, (GLsizei)(s_text_quads * 6u));
    }
}
