/*
 * gl_portability.h - OpenGL SC 2.0 / OpenGL ES 2.0 header selection.
 *
 * OpenGL SC 2.0 is an API-compatible subset of OpenGL ES 2.0: every entry
 * point used by this UI exists in both profiles with identical signatures,
 * so SC-conformant code compiles unchanged against either header.  The dev
 * build targets desktop GLES2/EGL; the safety-critical target build uses the
 * platform's OpenGL SC 2.0 header (Khronos SC profile, typically shipped as
 * GL/glsc2.h by a conformant SC 2.0 implementation - adjust the include
 * below to the exact header your SC toolchain provides).
 */
#ifndef VHMS_GL_PORTABILITY_H
#define VHMS_GL_PORTABILITY_H

#ifdef VHMS_UI_DEV_BUILD
#include <GLES2/gl2.h>
#else
/* OpenGL SC 2.0 implementation header, e.g. the Khronos GL/glsc2.h-style
 * header shipped with the SC 2.0 runtime for the target. */
#include <GL/glsc2.h>
#endif

#endif /* VHMS_GL_PORTABILITY_H */
