/*
 * screens.h - screen state machine for the VHMS maintenance UI.
 *
 * Three screens: fleet overview -> vehicle detail -> issue detail.
 * Key codes come from platform_egl.h (VHMS_KEY_* or ASCII).
 */
#ifndef VHMS_SCREENS_H
#define VHMS_SCREENS_H

void vhms_screens_init(void);

/* Route one key press.  Returns 1 when the user asked to quit, else 0. */
int vhms_screens_handle_key(int key);

/* Render the current screen into a fresh frame. */
void vhms_screens_render(int width, int height);

#endif /* VHMS_SCREENS_H */
