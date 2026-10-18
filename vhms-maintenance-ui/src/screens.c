/*
 * screens.c - fleet / vehicle / issue screens on top of ui_core.
 *
 * All layout derives from the framebuffer size each frame; there are no
 * fixed pixel positions outside the top/bottom bars.  Text that can exceed
 * its column is drawn with ui_draw_text_bounded(); body copy is word-wrapped
 * into a static line buffer.
 */
#define _POSIX_C_SOURCE 200809L /* gmtime_r */

#include "screens.h"
#include "platform_egl.h"
#include "ui_core.h"
#include "data_model.h"

#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

enum {
    SCR_FLEET = 0,
    SCR_VEHICLE,
    SCR_ISSUE
};

#define TOP_BAR_H 46.0f
#define BOT_BAR_H 34.0f

#define SCR_MAX_LINES 96
#define SCR_LINE_LEN  120

typedef struct {
    char text[SCR_LINE_LEN];
    float scale;
    ui_color_t color;
} scr_line_t;

static struct {
    int screen;
    int fleet_sel;
    int fleet_scroll;
    int veh_sel;
    int veh_scroll;
    int issue_scroll;
} s_st;

/* ---- palette (dark cockpit theme) ---- */
static ui_color_t col_panel(void)     { return ui_rgba(0.078f, 0.094f, 0.133f, 1.0f); }
static ui_color_t col_panel_sel(void) { return ui_rgba(0.135f, 0.165f, 0.225f, 1.0f); }
static ui_color_t col_text(void)      { return ui_rgba(0.920f, 0.940f, 0.970f, 1.0f); }
static ui_color_t col_dim(void)       { return ui_rgba(0.550f, 0.610f, 0.700f, 1.0f); }
static ui_color_t col_dark(void)      { return ui_rgba(0.040f, 0.050f, 0.070f, 1.0f); }
static ui_color_t col_accent(void)    { return ui_rgba(0.300f, 0.550f, 1.000f, 1.0f); }

static ui_color_t col_severity(vhms_severity_t s)
{
    switch (s) {
    case VHMS_SEV_OK:       return ui_rgba(0.200f, 0.780f, 0.380f, 1.0f);
    case VHMS_SEV_ADVISORY:  return ui_rgba(1.000f, 0.780f, 0.250f, 1.0f);
    case VHMS_SEV_CAUTION:   return ui_rgba(1.000f, 0.550f, 0.160f, 1.0f);
    case VHMS_SEV_CRITICAL:  return ui_rgba(1.000f, 0.300f, 0.300f, 1.0f);
    default:                 return ui_rgba(0.500f, 0.500f, 0.500f, 1.0f);
    }
}

/* ---- small formatting helpers (stack buffers only) ---- */

static void fmt_odometer(unsigned int km, char *buf, size_t n)
{
    char digits[16];
    char out[24];
    size_t dl, i, o = 0, first;

    snprintf(digits, sizeof(digits), "%u", km);
    dl = strlen(digits);
    first = dl % 3u;
    if (first == 0u)
        first = 3u;
    for (i = 0; i < dl && o + 2u < sizeof(out); i++) {
        out[o++] = digits[i];
        if (i + 1u == first || (i + 1u > first && (i + 1u - first) % 3u == 0u)) {
            if (i + 1u != dl)
                out[o++] = ',';
        }
    }
    out[o] = '\0';
    snprintf(buf, n, "%s km", out);
}

static void fmt_clock_utc(char *buf, size_t n)
{
    time_t t = time(NULL);
    struct tm tmv;
    gmtime_r(&t, &tmv);
    strftime(buf, n, "%H:%M:%S UTC", &tmv);
}

/* ---- wrapped-line buffer ---- */

static int scr_push_line(scr_line_t *lines, int n, float scale,
                         ui_color_t color, const char *text)
{
    size_t len;

    if (n >= SCR_MAX_LINES)
        return n;
    len = strlen(text);
    if (len > (size_t)(SCR_LINE_LEN - 1))
        len = (size_t)(SCR_LINE_LEN - 1);
    memcpy(lines[n].text, text, len);
    lines[n].text[len] = '\0';
    lines[n].scale = scale;
    lines[n].color = color;
    return n + 1;
}

static int scr_add_wrapped(scr_line_t *lines, int n, const char *src,
                           float scale, ui_color_t color, float max_w)
{
    char cur[SCR_LINE_LEN];
    size_t clen = 0;
    const char *p = src;
    float adv = 8.0f * scale;
    size_t max_chars = adv > 0.0f ? (size_t)(max_w / adv) : 8u;

    if (max_chars < 8u)
        max_chars = 8u;
    if (max_chars > (size_t)(SCR_LINE_LEN - 1))
        max_chars = (size_t)(SCR_LINE_LEN - 1);

    cur[0] = '\0';
    while (*p && n < SCR_MAX_LINES) {
        const char *wstart;
        size_t wlen = 0, chunk;

        while (*p == ' ')
            p++;
        if (!*p)
            break;
        wstart = p;
        while (*p && *p != ' ') {
            p++;
            wlen++;
        }
        while (wlen > 0u) {
            chunk = wlen > max_chars ? max_chars : wlen;
            if (clen > 0u && clen + 1u + chunk > max_chars) {
                n = scr_push_line(lines, n, scale, color, cur);
                clen = 0u;
                cur[0] = '\0';
            }
            if (clen > 0u)
                cur[clen++] = ' ';
            assert(clen + chunk < sizeof(cur));
            memcpy(cur + clen, wstart, chunk);
            clen += chunk;
            cur[clen] = '\0';
            wstart += chunk;
            wlen -= chunk;
        }
    }
    if (clen > 0u)
        n = scr_push_line(lines, n, scale, color, cur);
    return n;
}

/* ---- chrome ---- */

static void scr_top_bar(float w)
{
    char clock[32];
    char badge[32];
    float bw;

    fmt_clock_utc(clock, sizeof(clock));
    ui_draw_rect(0.0f, 0.0f, w, TOP_BAR_H, col_panel());
    ui_draw_text(20.0f, 13.0f, 2.0f, col_text(), "VHMS MAINTENANCE");

    snprintf(badge, sizeof(badge), "CRIT %u",
             (unsigned)fleet_critical_count());
    bw = ui_text_width(badge, 1.0f);
    ui_draw_rect(w - bw - 150.0f, 9.0f, bw + 20.0f, 28.0f,
                 ui_rgba(1.0f, 0.30f, 0.30f, 1.0f));
    ui_draw_text(w - bw - 140.0f, 15.0f, 1.0f, col_dark(), badge);
    ui_draw_text(w - 130.0f, 15.0f, 1.0f, col_dim(), clock);

    ui_draw_rect(0.0f, TOP_BAR_H, w, 2.0f, col_panel_sel());
}

static void scr_bottom_bar(float w, float h)
{
    ui_draw_rect(0.0f, h - BOT_BAR_H, w, 2.0f, col_panel_sel());
    ui_draw_rect(0.0f, h - BOT_BAR_H + 2.0f, w, BOT_BAR_H - 2.0f,
                 col_panel());
    ui_draw_text_bounded(20.0f, h - 24.0f, 1.0f, col_dim(),
        "Up/Down navigate   Enter select   Esc back   A acknowledge   Q quit",
        w - 40.0f);
}

/* ---- fleet overview ---- */

static void scr_render_fleet(float w, float h)
{
    size_t count = fleet_vehicle_count();
    size_t i, vis0, vis1;
    float y0 = TOP_BAR_H + 54.0f;
    float row_h = 60.0f;
    int visible;
    char sub[96];
    char odo[32];
    char open[32];
    char title[64];

    snprintf(title, sizeof(title), "FLEET OVERVIEW (%u vehicles)",
             (unsigned)count);
    ui_draw_text(24.0f, TOP_BAR_H + 16.0f, 2.0f, col_text(), title);

    visible = (int)((h - BOT_BAR_H - y0) / row_h);
    if (visible < 1)
        visible = 1;
    if (s_st.fleet_sel < 0)
        s_st.fleet_sel = 0;
    if ((size_t)s_st.fleet_sel >= count)
        s_st.fleet_sel = (int)count - 1;
    if (s_st.fleet_sel < s_st.fleet_scroll)
        s_st.fleet_scroll = s_st.fleet_sel;
    if (s_st.fleet_sel >= s_st.fleet_scroll + visible)
        s_st.fleet_scroll = s_st.fleet_sel - visible + 1;

    vis0 = (size_t)s_st.fleet_scroll;
    vis1 = vis0 + (size_t)visible;
    if (vis1 > count)
        vis1 = count;

    for (i = vis0; i < vis1; i++) {
        const vhms_vehicle_t *v = fleet_vehicle_at(i);
        float y = y0 + (float)(i - vis0) * row_h;
        float chip_w;
        const char *sev = vhms_severity_name(v->status);
        ui_color_t sevc = col_severity(v->status);
        int selected = (int)i == s_st.fleet_sel;

        ui_draw_rect(20.0f, y, w - 40.0f, row_h - 6.0f,
                     selected ? col_panel_sel() : col_panel());
        if (selected)
            ui_draw_rect(20.0f, y, 4.0f, row_h - 6.0f, col_accent());

        ui_draw_rect(38.0f, y + 20.0f, 14.0f, 14.0f, sevc);
        ui_draw_text(64.0f, y + 8.0f, 2.0f, col_text(), v->name);

        fmt_odometer(v->odometer_km, odo, sizeof(odo));
        snprintf(sub, sizeof(sub), "%s  |  %s", v->id, odo);
        ui_draw_text(64.0f, y + 32.0f, 1.0f, col_dim(), sub);

        chip_w = ui_text_width(sev, 1.0f);
        ui_draw_rect(w - 40.0f - chip_w - 20.0f, y + 6.0f, chip_w + 20.0f,
                     24.0f, sevc);
        ui_draw_text(w - 40.0f - chip_w - 10.0f, y + 12.0f, 1.0f,
                     col_dark(), sev);

        snprintf(open, sizeof(open), "%u open",
                 (unsigned)fleet_open_issue_count(i));
        ui_draw_text(w - 40.0f - ui_text_width(open, 1.0f), y + 36.0f, 1.0f,
                     col_dim(), open);
    }
}

/* ---- vehicle detail ---- */

static void scr_render_vehicle(float w, float h)
{
    const vhms_issue_t *issues[VHMS_MAX_ISSUES];
    const vhms_vehicle_t *v;
    size_t n, i, vis0, vis1;
    float y0 = TOP_BAR_H + 100.0f;
    float row_h = 48.0f;
    int visible;
    char sub[128];
    char odo[32];

    if (s_st.fleet_sel < 0 || (size_t)s_st.fleet_sel >= fleet_vehicle_count())
        return;
    v = fleet_vehicle_at((size_t)s_st.fleet_sel);
    n = fleet_issues_for_vehicle((size_t)s_st.fleet_sel, issues,
                                 VHMS_MAX_ISSUES);

    ui_draw_text(24.0f, TOP_BAR_H + 14.0f, 3.0f, col_text(), v->name);
    fmt_odometer(v->odometer_km, odo, sizeof(odo));
    snprintf(sub, sizeof(sub), "%s  |  %s  |  %u issue%s", v->id, odo,
             (unsigned)n, n == 1u ? "" : "s");
    ui_draw_text(24.0f, TOP_BAR_H + 52.0f, 1.0f, col_dim(), sub);
    ui_draw_text(24.0f, TOP_BAR_H + 76.0f, 1.0f, col_dim(), "ISSUES");

    if (n == 0u) {
        ui_draw_text(24.0f, y0, 1.0f, col_text(),
                     "No issues recorded for this vehicle.");
        return;
    }

    visible = (int)((h - BOT_BAR_H - y0) / row_h);
    if (visible < 1)
        visible = 1;
    if (s_st.veh_sel < 0)
        s_st.veh_sel = 0;
    if ((size_t)s_st.veh_sel >= n)
        s_st.veh_sel = (int)n - 1;
    if (s_st.veh_sel < s_st.veh_scroll)
        s_st.veh_scroll = s_st.veh_sel;
    if (s_st.veh_sel >= s_st.veh_scroll + visible)
        s_st.veh_scroll = s_st.veh_sel - visible + 1;

    vis0 = (size_t)s_st.veh_scroll;
    vis1 = vis0 + (size_t)visible;
    if (vis1 > n)
        vis1 = n;

    for (i = vis0; i < vis1; i++) {
        const vhms_issue_t *is = issues[i];
        float y = y0 + (float)(i - vis0) * row_h;
        const char *sev = vhms_severity_name(is->severity);
        ui_color_t sevc = col_severity(is->severity);
        int selected = (int)i == s_st.veh_sel;

        ui_draw_rect(20.0f, y, w - 40.0f, row_h - 6.0f,
                     selected ? col_panel_sel() : col_panel());
        if (selected)
            ui_draw_rect(20.0f, y, 4.0f, row_h - 6.0f, col_accent());

        ui_draw_rect(32.0f, y + 11.0f, 92.0f, 20.0f, sevc);
        ui_draw_text_bounded(40.0f, y + 16.0f, 1.0f, col_dark(), sev, 76.0f);

        ui_draw_text(136.0f, y + 4.0f, 2.0f, col_text(), is->dtc);
        if (is->acknowledged)
            ui_draw_text(136.0f + ui_text_width(is->dtc, 2.0f) + 12.0f,
                         y + 10.0f, 1.0f, col_dim(), "[ACK]");
        ui_draw_text_bounded(136.0f, y + 26.0f, 1.0f, col_dim(), is->title,
                             w - 136.0f - 30.0f);
    }
}

/* ---- issue detail ---- */

static void scr_render_issue(float w, float h)
{
    const vhms_issue_t *issues[VHMS_MAX_ISSUES];
    const vhms_issue_t *is;
    size_t n;
    scr_line_t lines[SCR_MAX_LINES];
    int nlines = 0, i;
    float body_y = TOP_BAR_H + 132.0f;
    float line_h = 14.0f;
    int visible, max_scroll;
    char banner[96];
    unsigned p;

    if (s_st.fleet_sel < 0 || (size_t)s_st.fleet_sel >= fleet_vehicle_count())
        return;
    n = fleet_issues_for_vehicle((size_t)s_st.fleet_sel, issues,
                                 VHMS_MAX_ISSUES);
    if (s_st.veh_sel < 0 || (size_t)s_st.veh_sel >= n)
        return;
    is = issues[(size_t)s_st.veh_sel];

    ui_draw_text(24.0f, TOP_BAR_H + 12.0f, 4.0f, col_text(), is->dtc);

    snprintf(banner, sizeof(banner), "%s  -  %s",
             vhms_severity_name(is->severity),
             is->acknowledged ? "ACKNOWLEDGED"
                              : "UNACKNOWLEDGED (press A)");
    ui_draw_rect(20.0f, TOP_BAR_H + 58.0f, w - 40.0f, 30.0f,
                 col_severity(is->severity));
    ui_draw_text_bounded(32.0f, TOP_BAR_H + 66.0f, 1.0f, col_dark(), banner,
                         w - 64.0f);

    ui_draw_text_bounded(24.0f, TOP_BAR_H + 102.0f, 2.0f, col_text(),
                         is->title, w - 48.0f);

    nlines = scr_push_line(lines, nlines, 1.0f, col_dim(), "DESCRIPTION");
    nlines = scr_add_wrapped(lines, nlines, is->description, 1.0f,
                             col_text(), w - 48.0f);
    nlines = scr_push_line(lines, nlines, 1.0f, col_text(), "");
    nlines = scr_push_line(lines, nlines, 1.0f, col_dim(),
                           "RECOMMENDED ACTION");
    nlines = scr_add_wrapped(lines, nlines, is->recommended_action, 1.0f,
                             col_text(), w - 48.0f);
    nlines = scr_push_line(lines, nlines, 1.0f, col_text(), "");
    nlines = scr_push_line(lines, nlines, 1.0f, col_dim(), "PARTS");
    if (is->parts_count == 0u) {
        nlines = scr_push_line(lines, nlines, 1.0f, col_dim(),
                               "No parts listed.");
    } else {
        for (p = 0; p < is->parts_count && p < VHMS_MAX_PARTS; p++) {
            char pl[VHMS_PART_LEN + 4];
            snprintf(pl, sizeof(pl), "- %s", is->parts[p]);
            nlines = scr_push_line(lines, nlines, 1.0f, col_text(), pl);
        }
    }

    visible = (int)((h - BOT_BAR_H - 8.0f - body_y) / line_h);
    if (visible < 1)
        visible = 1;
    max_scroll = nlines > visible ? nlines - visible : 0;
    if (s_st.issue_scroll < 0)
        s_st.issue_scroll = 0;
    if (s_st.issue_scroll > max_scroll)
        s_st.issue_scroll = max_scroll;

    for (i = s_st.issue_scroll;
         i < nlines && i < s_st.issue_scroll + visible; i++) {
        float y = body_y + (float)(i - s_st.issue_scroll) * line_h;
        if (lines[i].text[0] != '\0')
            ui_draw_text(24.0f, y, lines[i].scale, lines[i].color,
                         lines[i].text);
    }
}

/* ---- public API ---- */

void vhms_screens_init(void)
{
    memset(&s_st, 0, sizeof(s_st));
    s_st.screen = SCR_FLEET;
}

int vhms_screens_handle_key(int key)
{
    size_t count = fleet_vehicle_count();
    const vhms_issue_t *issues[VHMS_MAX_ISSUES];
    size_t n = 0;

    if (s_st.screen != SCR_FLEET &&
        s_st.fleet_sel >= 0 && (size_t)s_st.fleet_sel < count)
        n = fleet_issues_for_vehicle((size_t)s_st.fleet_sel, issues,
                                     VHMS_MAX_ISSUES);

    switch (s_st.screen) {
    case SCR_FLEET:
        if (key == VHMS_KEY_UP || key == 'w' || key == 'W') {
            if (s_st.fleet_sel > 0)
                s_st.fleet_sel--;
        } else if (key == VHMS_KEY_DOWN || key == 's' || key == 'S') {
            if ((size_t)s_st.fleet_sel + 1u < count)
                s_st.fleet_sel++;
        } else if (key == VHMS_KEY_ENTER) {
            s_st.screen = SCR_VEHICLE;
            s_st.veh_sel = 0;
            s_st.veh_scroll = 0;
        } else if (key == 'q' || key == 'Q') {
            return 1;
        }
        break;

    case SCR_VEHICLE:
        if (key == VHMS_KEY_UP || key == 'w' || key == 'W') {
            if (s_st.veh_sel > 0)
                s_st.veh_sel--;
        } else if (key == VHMS_KEY_DOWN || key == 's' || key == 'S') {
            if ((size_t)s_st.veh_sel + 1u < n)
                s_st.veh_sel++;
        } else if (key == VHMS_KEY_ENTER) {
            if (n > 0u) {
                s_st.screen = SCR_ISSUE;
                s_st.issue_scroll = 0;
            }
        } else if (key == VHMS_KEY_ESC) {
            s_st.screen = SCR_FLEET;
        } else if (key == 'q' || key == 'Q') {
            return 1;
        }
        break;

    case SCR_ISSUE:
        if (key == VHMS_KEY_UP) {
            if (s_st.issue_scroll > 0)
                s_st.issue_scroll--;
        } else if (key == VHMS_KEY_DOWN) {
            s_st.issue_scroll++;
        } else if (key == 'a' || key == 'A') {
            if ((size_t)s_st.veh_sel < n)
                fleet_toggle_acknowledged(issues[(size_t)s_st.veh_sel]);
        } else if (key == VHMS_KEY_ESC) {
            s_st.screen = SCR_VEHICLE;
        } else if (key == 'q' || key == 'Q') {
            return 1;
        }
        break;

    default:
        s_st.screen = SCR_FLEET;
        break;
    }
    return 0;
}

void vhms_screens_render(int width, int height)
{
    float w = (float)width;
    float h = (float)height;

    ui_begin_frame(width, height);
    scr_top_bar(w);
    scr_bottom_bar(w, h);
    switch (s_st.screen) {
    case SCR_VEHICLE:
        scr_render_vehicle(w, h);
        break;
    case SCR_ISSUE:
        scr_render_issue(w, h);
        break;
    case SCR_FLEET:
    default:
        scr_render_fleet(w, h);
        break;
    }
    ui_end_frame();
}
