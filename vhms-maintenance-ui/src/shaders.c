/*
 * shaders.c - compile/link the single UI quad program (GLSL ES 1.00).
 *
 * All compile/link/validation work happens here at init time; the frame
 * loop only calls glUseProgram and glUniform*.
 */
#include "shaders.h"

#include <stdio.h>

/* One program: textured-quad for glyphs, flat-color for rects, selected by
 * u_use_tex.  Explicit precision, no dynamic loop bounds, no vertex-shader
 * texture lookups - all required for the OpenGL SC 2.0 profile. */
static const char *VHMS_VERT_SRC =
    "attribute vec2 a_pos;\n"
    "attribute vec2 a_uv;\n"
    "attribute vec4 a_color;\n"
    "uniform mat4 u_proj;\n"
    "varying vec2 v_uv;\n"
    "varying vec4 v_color;\n"
    "void main() {\n"
    "    v_uv = a_uv;\n"
    "    v_color = a_color;\n"
    "    gl_Position = u_proj * vec4(a_pos, 0.0, 1.0);\n"
    "}\n";

static const char *VHMS_FRAG_SRC =
    "precision mediump float;\n"
    "varying vec2 v_uv;\n"
    "varying vec4 v_color;\n"
    "uniform sampler2D u_tex;\n"
    "uniform float u_use_tex;\n"
    "void main() {\n"
    "    float tex_a = texture2D(u_tex, v_uv).a;\n"
    "    float a = mix(1.0, tex_a, u_use_tex);\n"
    "    gl_FragColor = vec4(v_color.rgb, v_color.a * a);\n"
    "}\n";

static int vhms_compile(GLenum type, const char *src, GLuint *out)
{
    GLuint sh = glCreateShader(type);
    GLint ok = 0;

    glShaderSource(sh, 1, &src, NULL);
    glCompileShader(sh);
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024];
        glGetShaderInfoLog(sh, (GLsizei)sizeof(log), NULL, log);
        log[sizeof(log) - 1] = '\0';
        fprintf(stderr, "vhms: shader compile failed:\n%s\n", log);
        glDeleteShader(sh);
        return -1;
    }
    *out = sh;
    return 0;
}

int vhms_shaders_init(vhms_shaders_t *sh)
{
    GLuint vs = 0, fs = 0, prog = 0;
    GLint ok = 0;

    if (vhms_compile(GL_VERTEX_SHADER, VHMS_VERT_SRC, &vs) != 0)
        return -1;
    if (vhms_compile(GL_FRAGMENT_SHADER, VHMS_FRAG_SRC, &fs) != 0) {
        glDeleteShader(vs);
        return -1;
    }

    prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);

    /* Fixed attribute locations keep the vertex layout contract explicit. */
    glBindAttribLocation(prog, 0, "a_pos");
    glBindAttribLocation(prog, 1, "a_uv");
    glBindAttribLocation(prog, 2, "a_color");

    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024];
        glGetProgramInfoLog(prog, (GLsizei)sizeof(log), NULL, log);
        log[sizeof(log) - 1] = '\0';
        fprintf(stderr, "vhms: program link failed:\n%s\n", log);
        glDeleteProgram(prog);
        glDeleteShader(vs);
        glDeleteShader(fs);
        return -1;
    }
    glDeleteShader(vs);
    glDeleteShader(fs);

    sh->program  = prog;
    sh->a_pos    = glGetAttribLocation(prog, "a_pos");
    sh->a_uv     = glGetAttribLocation(prog, "a_uv");
    sh->a_color  = glGetAttribLocation(prog, "a_color");
    sh->u_proj   = glGetUniformLocation(prog, "u_proj");
    sh->u_tex    = glGetUniformLocation(prog, "u_tex");
    sh->u_use_tex = glGetUniformLocation(prog, "u_use_tex");

    if (sh->a_pos < 0 || sh->a_uv < 0 || sh->a_color < 0 ||
        sh->u_proj < 0 || sh->u_tex < 0 || sh->u_use_tex < 0) {
        fprintf(stderr, "vhms: program is missing an attribute/uniform\n");
        vhms_shaders_shutdown(sh);
        return -1;
    }

    glUseProgram(prog);
    glUniform1i(sh->u_tex, 0); /* font atlas lives on texture unit 0 */

    return 0;
}

void vhms_shaders_shutdown(vhms_shaders_t *sh)
{
    if (sh->program) {
        glDeleteProgram(sh->program);
        sh->program = 0;
    }
}
