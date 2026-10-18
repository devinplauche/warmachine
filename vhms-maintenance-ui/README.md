# vhms-maintenance-ui

A vehicle maintenance-issues dashboard frontend rendered with the **OpenGL SC 2.0**
API, written in C99. Think certifiable in-vehicle HMI: deterministic, statically
allocated, no heap use in the frame loop.

It shows a fleet overview, per-vehicle issue lists, and issue detail screens
(DTC codes, severity, recommended actions, parts) in a dark cockpit theme.

## Why OpenGL SC 2.0

OpenGL SC 2.0 is the safety-critical subset of OpenGL ES 2.0, defined by Khronos
for avionics/automotive displays that must pass DO-178C / ISO 26262
certification. It keeps the programmable pipeline (shaders, VBOs, textures) and
removes the parts that make certification painful. This codebase is written
against exactly that subset:

- **Fixed pipeline state.** One shader program, two VBOs, one texture. The
  frame loop is `glUseProgram` → upload two buffers → **two `glDrawArrays`
  calls** (rects, then text). No state thrash, no surprises.
- **Static everything.** All vertex storage is `static` with compile-time
  maximums (`UI_MAX_GLYPHS`, `UI_MAX_RECTS`). No `malloc`/`free` anywhere in
  the frame path — or anywhere at all, except the C library's own startup.
- **Init-time GL queries only.** `glGetUniformLocation`/`glGetAttribLocation`
  and shader compile logs run once at startup. The frame loop contains no
  `glGetError`, no `glGet*` — nothing that can stall the pipeline or branch
  on driver state.
- **Constrained shaders (GLSL ES 1.00).** Explicit `precision` qualifiers, no
  dynamic loop bounds, no vertex-shader texture lookups.
- **Deterministic text.** One 128×128 font-atlas texture (a real 8×8 bitmap
  font, ASCII 32–126) is uploaded once; per-frame text becomes quads in a
  static vertex buffer. No font rasterization at runtime.
- **Assert-bounded loops, no recursion.** Every loop over variable-length
  data carries a compile-time bound checked with `assert`.

## Dev build vs SC target build

| | Dev build (`VHMS_UI_DEV_BUILD=ON`, default) | SC target build (`OFF`) |
|---|---|---|
| GL header | system `<GLES2/gl2.h>` | target's OpenGL SC 2.0 header (`GL/glsc2.h`-style) |
| GL library | system `libGLESv2` | `VHMS_UI_SC_LIBRARY` (cache var) |
| Window system | EGL pbuffer, no X11 needed | **you replace `src/platform_egl.c`** |
| Keyboard | stdin (raw mode on a TTY) | platform keypad / touchscreen driver |
| Extra | `VHMS_UI_PPM_DUMP=<path>` writes one PPM per frame (headless test) | not compiled in |

The SC API is a strict subset of ES 2.0, so SC-conformant code builds against
either header unchanged — see `src/gl_portability.h`. `src/platform_egl.c`
contains an intentional `#error` when `VHMS_UI_DEV_BUILD` is off: it is the
dev/EGL implementation and must be replaced by the platform's display driver.
The contract it must implement is the four functions in
`src/platform_egl.h`.

## Build

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

Requires a C99 compiler, CMake ≥ 3.16, and EGL + GLES2 dev headers
(`libegl-dev`, `libgles-dev` on Debian/Ubuntu).

Run:

```bash
./build/vhms_maintenance_ui
```

Keys: `Up/Down` (or `W/S`, or arrow keys) navigate, `Enter` select,
`Esc` back, `A` acknowledge an issue, `Q` quit. The EGL surface is a headless
pbuffer — on a workstation you won't *see* a window; point
`VHMS_UI_PPM_DUMP=/tmp/frame.ppm` at it to capture frames, or run it in a
terminal for the interactive key loop.

SC-target configure (documents the expected toolchain variables):

```bash
cmake -S . -B build-sc -DVHMS_UI_DEV_BUILD=OFF \
  -DVHMS_UI_SC_INCLUDE_DIR=/opt/sc-sdk/include \
  -DVHMS_UI_SC_LIBRARY=/opt/sc-sdk/lib/libGLSC2.a
# then drop in the platform display driver replacing src/platform_egl.c
```

## Screen tour

- **Top bar (persistent):** app title, live UTC clock (1 Hz tick), and a red
  `CRIT n` badge counting unacknowledged CRITICAL issues fleet-wide.
- **Bottom bar (persistent):** key hints.
- **Fleet overview:** one row per vehicle — name, ID, odometer, a severity
  lamp + worst-severity color chip, and the open-issue count. Selected row is
  highlighted with an accent bar.
- **Vehicle detail:** header (name / ID / odometer / issue count) plus a
  scrollable issue list — severity chip, DTC, title, `[ACK]` marker for
  acknowledged issues.
- **Issue detail:** big DTC, full-width severity banner (shows acknowledged
  state, toggled with `A`), title, word-wrapped description, recommended
  action, and the parts list.

Severity colors: green OK, amber ADVISORY, orange CAUTION, red CRITICAL, on a
near-black (`#0b0e14`) cockpit background. All layout derives from the
framebuffer size each frame — no fixed pixel positions outside the bars.

## Data model

```c
Vehicle { id[16], name[48], odometer_km, status /* worst severity */ }
MaintenanceIssue {
  dtc[8], title[64], system[32] /* powertrain/chassis/electrical/body */,
  severity /* OK/ADVISORY/CAUTION/CRITICAL */,
  description[256], recommended_action[256],
  parts[4][48], parts_count, acknowledged
}
```

The demo fleet is 6 vehicles / 14 issues with realistic DTCs (P0300, P0420,
C1234, U0100, B0020, P0171, C0035, U0415, P0562, B1342, P0700, C0561, U0121,
P0442), mixed severities and odometer readings. A vehicle's `status` is the
worst severity across *all* its issues — an acknowledged fault is still a
fault; the `CRIT` badge counts only *unacknowledged* CRITICAL issues (things
still needing attention).

## Wiring a real VHMS feed

The seam is exactly one function: **`fleet_data_load()` in
`src/data_model.c`**. Everything the UI renders comes from the static tables
it fills; no screen code touches the tables directly, and the read API
(`fleet_vehicle_at`, `fleet_issues_for_vehicle`, `fleet_critical_count`, …)
is the only thing screens call.

To connect a live feed:

1. Replace the body of `fleet_data_load()` with the ingestion path — CAN/J1939
   DTC frames, UDS `19 02` (reportDTCByStatusMask) responses, or the
   telematics back-end poll.
2. Keep every string within its `*_LEN` bound and every table within its
   `VHMS_MAX_*` bound; recompute per-vehicle `status` as the worst issue
   severity (the loop at the end of the current implementation).
3. Map the feed's severity onto `vhms_severity_t`. Suggested mapping:
   confirmed emissions DTC → CAUTION, MIL-on powertrain fault → CRITICAL,
   SRS/ABS active fault → CRITICAL, pending/history-only → ADVISORY.
4. For a streaming feed, add a `fleet_data_refresh()` that updates the tables
   in place (still no heap) and call it on your telemetry cadence; the
   screens re-read the tables every frame, so no UI changes are needed.
5. `acknowledged` is operator state — persist it outside the feed refresh
   (key it by vehicle id + DTC) so an update cycle doesn't clear it.

## Files

| File | Purpose |
|---|---|
| `src/main.c` | init order, 20 Hz event pump, render-on-input + 1 Hz clock tick, shutdown |
| `src/gl_portability.h` | GLES2 vs OpenGL SC 2.0 header selection |
| `src/shaders.{h,c}` | single quad shader program (GLSL ES 1.00), init-time compile/link |
| `src/font_8x8.h` | real 8×8 bitmap font, ASCII 32–126 |
| `src/ui_core.{h,c}` | static-buffer 2D renderer: rects, text, ellipsized bounded text |
| `src/data_model.{h,c}` | fleet/vehicle/issue tables + read API; **the feed seam** |
| `src/screens.{h,c}` | fleet → vehicle → issue state machine + layout |
| `src/platform_egl.{h,c}` | dev EGL/inputs; **replaced on the SC target** |

## Deliberate simplifications (prototype)

- The demo fleet is compiled in; see above for the feed seam.
- Keyboard-only input in the dev build; touch targets are a target-platform
  concern.
- `VHMS_UI_PPM_DUMP` uses `glReadPixels` (a dev-only aid, never in the SC
  frame path) and `fopen`/`fwrite` — fine for a dev harness, not for the
  target.
- The EGL platform layer uses POSIX (`fcntl`, `termios`, `nanosleep`);
  the SC target replaces the whole file.
