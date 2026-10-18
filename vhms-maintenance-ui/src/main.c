/*
 * main.c - VHMS maintenance-issues dashboard.
 *
 * init: platform -> shaders -> ui -> data.  Then a fixed-cadence event pump
 * (20 Hz poll) that re-renders on input plus a 1 Hz tick for the UTC clock.
 * No heap allocation anywhere in the loop; shutdown is the reverse of init.
 */
#define _POSIX_C_SOURCE 200809L /* nanosleep */

#include "platform_egl.h"
#include "ui_core.h"
#include "screens.h"
#include "data_model.h"

#include <stdio.h>
#include <time.h>

#define VHMS_WIN_W 960
#define VHMS_WIN_H 600

int main(void)
{
    int quit = 0;
    int dirty = 1;
    long last_sec = -1;
    struct timespec poll = { 0, 50 * 1000 * 1000 }; /* 20 Hz pump */

    if (platform_init(VHMS_WIN_W, VHMS_WIN_H) != 0) {
        fprintf(stderr, "vhms: platform_init failed\n");
        return 1;
    }
    if (ui_init() != 0) {
        fprintf(stderr, "vhms: ui_init failed\n");
        platform_shutdown();
        return 1;
    }
    fleet_data_load();
    vhms_screens_init();

    while (!quit) {
        int key;
        long now_sec;

        while ((key = platform_poll_key()) != VHMS_KEY_NONE) {
            if (vhms_screens_handle_key(key)) {
                quit = 1;
                break;
            }
            dirty = 1;
        }
        if (quit)
            break;

        now_sec = (long)time(NULL);
        if (now_sec != last_sec) {
            last_sec = now_sec;
            dirty = 1; /* 1 Hz clock tick */
        }
        if (dirty) {
            vhms_screens_render(VHMS_WIN_W, VHMS_WIN_H);
            platform_present();
            dirty = 0;
        }
        nanosleep(&poll, NULL);
    }

    ui_shutdown();
    platform_shutdown();
    return 0;
}
