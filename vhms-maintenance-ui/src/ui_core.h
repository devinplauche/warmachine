/*
 * ui_core.h - 2D immediate-style renderer on top of the quad shader.
 *
 * Contract for the safety-critical target:
 *  - All vertex storage is static with compile-time maximums.
 *  - No heap allocation anywhere in this module, init included.
 *  - The frame path makes exactly two glDrawArrays calls (rects, text).
 *  - No glGetError / glGet* queries in the frame path.
 */
#ifndef VHMS_UI_CORE_H
#define VHMS_UI_CORE_H

#include "shaders.h"

#define UI_MAX_GLYPHS 2048u
#define UI_MAX_RECTS  1024u

typedef struct {
    float r, g, b, a;
} ui_color_t;

static inline ui_color_t ui_rgba(float r, float g, float b, float a)
{
    ui_color_t c = { r, g, b, a };
    return c;
}

/* One-time setup: shaders, VBOs, font-atlas texture upload.
 * Returns 0 on success, -1 on failure. */
int ui_init(void);
void ui_shutdown(void);

/* Start a frame: viewport, clear, reset batch counters. */
void ui_begin_frame(int width, int height);

/* Append a filled rectangle to the rect batch (clipped by caller). */
void ui_draw_rect(float x, float y, float w, float h, ui_color_t color);

/* Append text; scale multiplies the 8x8 glyph cell. */
void ui_draw_text(float x, float y, float scale, ui_color_t color,
                 const char *str);

/* Like ui_draw_text, but never exceeds max_width_px: overlong strings are
 * ellipsized ("...").  Pass max_width_px <= 0 to draw nothing. */
void ui_draw_text_bounded(float x, float y, float scale, ui_color_t color,
                          const char *str, float max_width_px);

/* Pixel width of str at the given scale. */
float ui_text_width(const char *str, float scale);

/* Upload both batches and issue the two draw calls. */
void ui_end_frame(void);

#endif /* VHMS_UI_CORE_H */
