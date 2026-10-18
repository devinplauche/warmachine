/*
 * platform_egl.h - minimal platform abstraction for the dev build.
 *
 * The OpenGL SC 2.0 target replaces platform_egl.c with the platform's own
 * display driver; this header is the contract it must implement.
 */
#ifndef VHMS_PLATFORM_EGL_H
#define VHMS_PLATFORM_EGL_H

/* Key codes returned by platform_poll_key(): ASCII for printable keys,
 * VHMS_KEY_* for special keys. */
enum {
    VHMS_KEY_NONE = 0,
    VHMS_KEY_UP = 256,
    VHMS_KEY_DOWN,
    VHMS_KEY_LEFT,
    VHMS_KEY_RIGHT,
    VHMS_KEY_ENTER,
    VHMS_KEY_ESC
};

/* Create the GL context + drawing surface.  Returns 0 on success. */
int platform_init(int width, int height);

/* Non-blocking: one pending key per call, VHMS_KEY_NONE when idle. */
int platform_poll_key(void);

/* Present the finished frame. */
void platform_present(void);

void platform_shutdown(void);

#endif /* VHMS_PLATFORM_EGL_H */
