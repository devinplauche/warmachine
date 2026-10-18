/*
 * platform_egl.c - DEV BUILD ONLY: EGL pbuffer + stdin keyboard.
 *
 * This is deliberately the simplest honest thing that works: an offscreen
 * EGL pbuffer (no X11/Wayland needed, runs headless under Mesa) and
 * non-blocking keyboard input from stdin.  Frames render on demand (on
 * input, plus a 1 Hz tick for the UTC clock).
 *
 * The OpenGL SC 2.0 safety target MUST replace this file with the
 * platform's display driver (framebuffer / display controller init,
 * vsync present, keypad or touchscreen input).  See the header for the
 * contract.
 */
#ifndef VHMS_UI_DEV_BUILD
#error "platform_egl.c is the dev/EGL implementation; the OpenGL SC 2.0 target must replace this file with the platform display driver (see platform_egl.h)."
#endif

#define _POSIX_C_SOURCE 200809L /* nanosleep, fileno, tcsetattr */

#include "platform_egl.h"
#include "gl_portability.h"

#ifdef VHMS_UI_DEV_BUILD
#include <EGL/egl.h>
#include <EGL/eglext.h>
#endif

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

static EGLDisplay s_dpy = EGL_NO_DISPLAY;
static EGLSurface s_surf = EGL_NO_SURFACE;
static EGLContext s_ctx = EGL_NO_CONTEXT;
static int s_width = 0;
static int s_height = 0;

/* stdin state, restored on shutdown */
static int s_stdin_flags = -1;
static struct termios s_term_orig;
static int s_term_saved = 0;

/* Pending input bytes (escape sequences arrive together). */
static unsigned char s_pending[64];
static size_t s_pending_n = 0;

static void platform_restore_input(void)
{
    if (s_stdin_flags >= 0) {
        fcntl(STDIN_FILENO, F_SETFL, s_stdin_flags);
        s_stdin_flags = -1;
    }
    if (s_term_saved) {
        tcsetattr(STDIN_FILENO, TCSANOW, &s_term_orig);
        s_term_saved = 0;
    }
}

int platform_init(int width, int height)
{
    EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8,
        EGL_NONE
    };
    EGLint pb_attribs[] = {
        EGL_WIDTH, 0,   /* patched below */
        EGL_HEIGHT, 0,
        EGL_NONE
    };
    EGLint ctx_attribs[] = {
        EGL_CONTEXT_CLIENT_VERSION, 2,
        EGL_NONE
    };
    EGLConfig cfg;
    EGLint ncfg = 0;
    struct termios raw;

    if (width <= 0 || height <= 0) {
        fprintf(stderr, "vhms: bad surface size %dx%d\n", width, height);
        return -1;
    }
    pb_attribs[1] = width;
    pb_attribs[3] = height;

    /* Prefer the surfaceless platform so the dev build runs with no
     * window system (headless CI, SSH sessions); fall back to the default
     * display otherwise. */
    {
        PFNEGLGETPLATFORMDISPLAYPROC get_platform_display =
            (PFNEGLGETPLATFORMDISPLAYPROC)eglGetProcAddress(
                "eglGetPlatformDisplay");
        if (get_platform_display) {
            s_dpy = get_platform_display(EGL_PLATFORM_SURFACELESS_MESA,
                                         EGL_DEFAULT_DISPLAY, NULL);
        }
    }
    if (s_dpy == EGL_NO_DISPLAY)
        s_dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (s_dpy == EGL_NO_DISPLAY) {
        fprintf(stderr, "vhms: no usable EGL display\n");
        return -1;
    }
    if (!eglInitialize(s_dpy, NULL, NULL)) {
        fprintf(stderr, "vhms: eglInitialize failed\n");
        return -1;
    }
    if (!eglChooseConfig(s_dpy, cfg_attribs, &cfg, 1, &ncfg) || ncfg < 1) {
        fprintf(stderr, "vhms: eglChooseConfig found no ES2 pbuffer config\n");
        goto fail;
    }
    s_surf = eglCreatePbufferSurface(s_dpy, cfg, pb_attribs);
    if (s_surf == EGL_NO_SURFACE) {
        fprintf(stderr, "vhms: eglCreatePbufferSurface failed\n");
        goto fail;
    }
    s_ctx = eglCreateContext(s_dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    if (s_ctx == EGL_NO_CONTEXT) {
        fprintf(stderr, "vhms: eglCreateContext failed\n");
        goto fail;
    }
    if (!eglMakeCurrent(s_dpy, s_surf, s_surf, s_ctx)) {
        fprintf(stderr, "vhms: eglMakeCurrent failed\n");
        goto fail;
    }
    s_width = width;
    s_height = height;

    /* Non-blocking stdin ... */
    s_stdin_flags = fcntl(STDIN_FILENO, F_GETFL, 0);
    if (s_stdin_flags >= 0)
        fcntl(STDIN_FILENO, F_SETFL, s_stdin_flags | O_NONBLOCK);
    /* ... and raw mode when stdin is a terminal (no-Enter keypresses). */
    if (tcgetattr(STDIN_FILENO, &s_term_orig) == 0) {
        s_term_saved = 1;
        raw = s_term_orig;
        raw.c_lflag &= (unsigned)~(ICANON | ECHO);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 0;
        tcsetattr(STDIN_FILENO, TCSANOW, &raw);
    }

    fprintf(stderr, "vhms: EGL pbuffer %dx%d ready (GL: %s)\n",
            width, height, glGetString(GL_VERSION));
    return 0;

fail:
    platform_shutdown();
    return -1;
}

/* Map one raw byte (plus lookahead for escape sequences) to a key code. */
static int platform_decode_key(void)
{
    unsigned char b;

    if (s_pending_n == 0)
        return VHMS_KEY_NONE;
    b = s_pending[0];
    memmove(s_pending, s_pending + 1, --s_pending_n);

    if (b == 0x1bu) { /* ESC or CSI sequence */
        if (s_pending_n >= 2 && s_pending[0] == '[') {
            unsigned char c = s_pending[1];
            memmove(s_pending, s_pending + 2, s_pending_n - 2);
            s_pending_n -= 2;
            switch (c) {
            case 'A': return VHMS_KEY_UP;
            case 'B': return VHMS_KEY_DOWN;
            case 'C': return VHMS_KEY_RIGHT;
            case 'D': return VHMS_KEY_LEFT;
            default:  return VHMS_KEY_ESC;
            }
        }
        return VHMS_KEY_ESC;
    }
    if (b == '\r' || b == '\n')
        return VHMS_KEY_ENTER;
    if (b == 0x7fu) /* backspace behaves as "back" */
        return VHMS_KEY_ESC;
    return (int)b;
}

int platform_poll_key(void)
{
    ssize_t r;

    if (s_pending_n == 0) {
        r = read(STDIN_FILENO, s_pending, sizeof(s_pending));
        if (r > 0)
            s_pending_n = (size_t)r;
        else
            return VHMS_KEY_NONE;
    }
    return platform_decode_key();
}

void platform_present(void)
{
    /* Dev-only framebuffer capture for headless verification:
     * VHMS_UI_PPM_DUMP=<path> writes one binary PPM per presented frame.
     * The capture runs BEFORE eglSwapBuffers: on pbuffer surfaces the
     * post-swap buffer contents are undefined. */
    const char *dump = getenv("VHMS_UI_PPM_DUMP");

    if (dump && dump[0] && s_width > 0 && s_height > 0) {
        /* NOTE: GL_RGBA readback - OpenGL ES 2.0 only guarantees RGBA; a
         * GL_RGB glReadPixels is rejected with INVALID_OPERATION on some
         * implementations (observed on Mesa). */
        static unsigned char px[1920 * 1080 * 4u];
        static unsigned char row[1920 * 3u];
        FILE *f;

        if ((size_t)s_width * (size_t)s_height * 4u <= sizeof(px) &&
            (size_t)s_width * 3u <= sizeof(row)) {
            size_t y;
            int x;
            glReadPixels(0, 0, s_width, s_height, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            f = fopen(dump, "wb");
            if (f) {
                /* PPM rows run top-down; GL rows run bottom-up. */
                fprintf(f, "P6\n%d %d\n255\n", s_width, s_height);
                for (y = 0; y < (size_t)s_height; y++) {
                    const unsigned char *src =
                        px + ((size_t)s_height - 1u - y) *
                             (size_t)s_width * 4u;
                    for (x = 0; x < s_width; x++) {
                        row[x * 3 + 0] = src[x * 4 + 0];
                        row[x * 3 + 1] = src[x * 4 + 1];
                        row[x * 3 + 2] = src[x * 4 + 2];
                    }
                    fwrite(row, 3u, (size_t)s_width, f);
                }
                fclose(f);
            }
        }
    }

    eglSwapBuffers(s_dpy, s_surf);
}

void platform_shutdown(void)
{
    platform_restore_input();
    if (s_dpy != EGL_NO_DISPLAY) {
        eglMakeCurrent(s_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        if (s_ctx != EGL_NO_CONTEXT) {
            eglDestroyContext(s_dpy, s_ctx);
            s_ctx = EGL_NO_CONTEXT;
        }
        if (s_surf != EGL_NO_SURFACE) {
            eglDestroySurface(s_dpy, s_surf);
            s_surf = EGL_NO_SURFACE;
        }
        eglTerminate(s_dpy);
        s_dpy = EGL_NO_DISPLAY;
    }
    s_width = 0;
    s_height = 0;
}
