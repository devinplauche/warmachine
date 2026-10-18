/*
 * data_model.c - static fleet tables and read API.
 *
 * WIRING A REAL FEED: replace fleet_data_load() with code that populates
 * s_vehicles / s_issues from the vehicle bus (CAN/J1939 DTC frames, UDS
 * responses) or the telematics back-end, then recomputes per-vehicle status.
 * Keep every string within its *_LEN bound and every table within its
 * VHMS_MAX_* bound; the read API and all screens stay untouched.
 */
#include "data_model.h"

#include <assert.h>
#include <string.h>

static vhms_vehicle_t s_vehicles[VHMS_MAX_VEHICLES];
static vhms_issue_t s_issues[VHMS_MAX_ISSUES];
static size_t s_vehicle_count = 0;
static size_t s_issue_count = 0;

static void dm_add_issue(int vehicle_idx,
                         const char *dtc,
                         const char *title,
                         const char *system,
                         vhms_severity_t severity,
                         const char *description,
                         const char *action,
                         const char *p0, const char *p1,
                         const char *p2, const char *p3,
                         int acknowledged)
{
    vhms_issue_t *is;
    const char *parts[VHMS_MAX_PARTS] = { p0, p1, p2, p3 };
    unsigned i;

    assert(s_issue_count < VHMS_MAX_ISSUES);
    assert(vehicle_idx >= 0 && (size_t)vehicle_idx < s_vehicle_count);
    assert(strlen(dtc) < VHMS_DTC_LEN);
    assert(strlen(title) < VHMS_TITLE_LEN);
    assert(strlen(system) < VHMS_SYSTEM_LEN);
    assert(strlen(description) < VHMS_DESC_LEN);
    assert(strlen(action) < VHMS_ACTION_LEN);

    is = &s_issues[s_issue_count++];
    memset(is, 0, sizeof(*is));
    is->vehicle_idx = vehicle_idx;
    strcpy(is->dtc, dtc);
    strcpy(is->title, title);
    strcpy(is->system, system);
    is->severity = severity;
    strcpy(is->description, description);
    strcpy(is->recommended_action, action);
    for (i = 0; i < VHMS_MAX_PARTS; i++) {
        if (parts[i]) {
            assert(strlen(parts[i]) < VHMS_PART_LEN);
            strcpy(is->parts[i], parts[i]);
            is->parts_count++;
        }
    }
    is->acknowledged = acknowledged ? 1 : 0;
}

void fleet_data_load(void)
{
    size_t v, i;

    s_vehicle_count = 6;
    assert(s_vehicle_count <= VHMS_MAX_VEHICLES);
    memset(s_vehicles, 0, sizeof(s_vehicles));

    strcpy(s_vehicles[0].id, "VH-101");
    strcpy(s_vehicles[0].name, "Rescue 1");
    s_vehicles[0].odometer_km = 48210;

    strcpy(s_vehicles[1].id, "VH-207");
    strcpy(s_vehicles[1].name, "Patrol 7");
    s_vehicles[1].odometer_km = 120550;

    strcpy(s_vehicles[2].id, "VH-312");
    strcpy(s_vehicles[2].name, "Medic 4");
    s_vehicles[2].odometer_km = 89320;

    strcpy(s_vehicles[3].id, "VH-418");
    strcpy(s_vehicles[3].name, "Engine 2");
    s_vehicles[3].odometer_km = 150230;

    strcpy(s_vehicles[4].id, "VH-509");
    strcpy(s_vehicles[4].name, "Utility 9");
    s_vehicles[4].odometer_km = 67100;

    strcpy(s_vehicles[5].id, "VH-603");
    strcpy(s_vehicles[5].name, "Command 3");
    s_vehicles[5].odometer_km = 204410;

    s_issue_count = 0;

    dm_add_issue(0, "P0300", "Cylinder 3 misfire detected", "powertrain",
        VHMS_SEV_CRITICAL,
        "PCM detected repeated misfire on cylinder 3 at idle and light load. "
        "Freeze frame shows the misfire counter climbing with the engine at "
        "operating temperature.",
        "Swap the cylinder 3 coil pack with cylinder 1 and re-test; if the "
        "misfire follows the coil, replace it. Inspect the spark plug and "
        "injector wiring while the cover is off.",
        "Ignition coil pack", "Spark plug (iridium)", NULL, NULL, 0);

    dm_add_issue(0, "P0420", "Catalyst efficiency below threshold",
        "powertrain", VHMS_SEV_ADVISORY,
        "Downstream O2 sensor waveform mirrors the upstream sensor at "
        "cruise, indicating reduced catalyst storage capacity on bank 1.",
        "Check for exhaust leaks ahead of the converter first. Monitor the "
        "pending status over two drive cycles before replacing the converter.",
        "Catalytic converter (bank 1)", "Exhaust gasket kit", NULL, NULL, 0);

    dm_add_issue(1, "P0442", "EVAP system small leak detected", "powertrain",
        VHMS_SEV_ADVISORY,
        "EVAP monitor flagged a 0.020 in equivalent leak during the last two "
        "cold-start drive cycles.",
        "Smoke-test the EVAP system. Inspect the gas cap seal and filler "
        "neck first - most small leaks trace back to the cap.",
        "Fuel cap", "EVAP purge valve", NULL, NULL, 1);

    dm_add_issue(2, "B0020", "Driver airbag deployment loop fault", "body",
        VHMS_SEV_CRITICAL,
        "SRS module reports high resistance on the driver airbag squib "
        "circuit. The airbag warning lamp is on and the system is disabled.",
        "Do NOT probe the squib with a multimeter. Disconnect the battery, "
        "wait 10 minutes, then inspect the clockspring connector under the "
        "steering column.",
        "Clockspring assembly", NULL, NULL, NULL, 0);

    dm_add_issue(2, "C1234", "Left front wheel speed signal plausibility",
        "chassis", VHMS_SEV_CAUTION,
        "ABS module sees intermittent dropouts from the left-front wheel "
        "speed sensor above 40 km/h. Traction control may intervene "
        "unexpectedly.",
        "Inspect the sensor tip and tone ring for debris or damage and check "
        "harness routing near the strut. Replace the sensor if dropouts "
        "persist.",
        "Wheel speed sensor (FL)", "Tone ring", NULL, NULL, 0);

    dm_add_issue(2, "B1342", "SRS module internal self-test fault", "body",
        VHMS_SEV_ADVISORY,
        "SRS module stored an internal self-test fault. The lamp is off and "
        "the system currently passes the key-on self test.",
        "Record the freeze data and monitor across ten key cycles. If the "
        "code returns active, the SRS module needs replacement by a "
        "certified technician.",
        "SRS control module", NULL, NULL, NULL, 0);

    dm_add_issue(3, "P0171", "System too lean (bank 1)", "powertrain",
        VHMS_SEV_CAUTION,
        "Long-term fuel trim sits at +18% at idle and falls toward zero "
        "under load - classic unmetered air entering downstream of the MAF "
        "sensor.",
        "Smoke-test the intake for vacuum leaks. Inspect the PCV hose and "
        "intake boot for cracks before suspecting the MAF sensor.",
        "Intake boot", "PCV hose", NULL, NULL, 0);

    dm_add_issue(3, "C0035", "Right front wheel speed sensor circuit",
        "chassis", VHMS_SEV_CAUTION,
        "Right-front wheel speed sensor circuit reads open during the key-on "
        "self test. ABS and stability control are degraded.",
        "Check the sensor connector for corrosion and measure harness "
        "continuity back to the ABS module.",
        "Wheel speed sensor (FR)", NULL, NULL, NULL, 0);

    dm_add_issue(4, "U0121", "Lost communication with ABS module",
        "electrical", VHMS_SEV_ADVISORY,
        "Intermittent U0121 stored in history; no active fault. The ABS lamp "
        "cycled once last week and has not returned.",
        "Inspect CAN harness ground points and the ABS module connector for "
        "moisture. Clear the code and monitor - escalate if it returns.",
        NULL, NULL, NULL, NULL, 1);

    dm_add_issue(5, "U0100", "Lost communication with ECM/PCM", "electrical",
        VHMS_SEV_CRITICAL,
        "Vehicle entered limp mode twice this week with U0100 active. "
        "Powertrain CAN traffic from the ECM drops out in 200-400 ms bursts.",
        "Check ECM power and ground feeds under load. Inspect CAN bus "
        "termination and the bulkhead connector for fretting corrosion.",
        "ECM relay", "CAN terminator (120 ohm)", NULL, NULL, 0);

    dm_add_issue(5, "P0562", "System voltage low", "electrical",
        VHMS_SEV_CAUTION,
        "ECM logged system voltage below 10.5 V during cranking on three "
        "consecutive starts. The battery is 4 years old.",
        "Load-test the battery and check alternator output at idle and "
        "2000 rpm. Clean the battery terminals.",
        "Battery (group 48)", "Serpentine belt", NULL, NULL, 0);

    dm_add_issue(5, "P0700", "Transmission control system malfunction",
        "powertrain", VHMS_SEV_CAUTION,
        "TCM requested MIL illumination. Stored alongside gear-ratio "
        "plausibility codes in the transmission module.",
        "Pull TCM-specific codes with a capable scan tool before condemning "
        "the transmission. Check fluid level and condition first.",
        "Transmission fluid (6 qt)", "Transmission filter kit",
        NULL, NULL, 0);

    dm_add_issue(5, "C0561", "System disabled information stored", "chassis",
        VHMS_SEV_ADVISORY,
        "Informational code: chassis systems recorded a disable event while "
        "U0100 was active. No independent fault.",
        "No direct repair - resolve the underlying U0100 communication "
        "fault, then clear and verify.",
        NULL, NULL, NULL, NULL, 0);

    dm_add_issue(5, "U0415", "Invalid data received from ABS module",
        "electrical", VHMS_SEV_ADVISORY,
        "ECM received implausible wheel-speed data from the ABS module "
        "during the U0100 dropout events.",
        "Companion code to U0100 - address the communication fault first. "
        "Do not replace the ABS module for this code alone.",
        NULL, NULL, NULL, NULL, 0);

    /* Per-vehicle status = worst severity across all its issues. */
    for (v = 0; v < s_vehicle_count; v++)
        s_vehicles[v].status = VHMS_SEV_OK;
    for (i = 0; i < s_issue_count; i++) {
        vhms_vehicle_t *veh = &s_vehicles[s_issues[i].vehicle_idx];
        if (s_issues[i].severity > veh->status)
            veh->status = s_issues[i].severity;
    }
}

size_t fleet_vehicle_count(void)
{
    return s_vehicle_count;
}

const vhms_vehicle_t *fleet_vehicle_at(size_t i)
{
    assert(i < s_vehicle_count);
    if (i >= s_vehicle_count)
        return NULL;
    return &s_vehicles[i];
}

size_t fleet_issue_count(void)
{
    return s_issue_count;
}

const vhms_issue_t *fleet_issue_at(size_t i)
{
    assert(i < s_issue_count);
    if (i >= s_issue_count)
        return NULL;
    return &s_issues[i];
}

size_t fleet_issues_for_vehicle(size_t vehicle_idx,
                                const vhms_issue_t **out, size_t out_cap)
{
    size_t n = 0, i;

    assert(out_cap <= VHMS_MAX_ISSUES);
    for (i = 0; i < s_issue_count && n < out_cap; i++) {
        if ((size_t)s_issues[i].vehicle_idx == vehicle_idx)
            out[n++] = &s_issues[i];
    }
    return n;
}

size_t fleet_critical_count(void)
{
    size_t n = 0, i;

    for (i = 0; i < s_issue_count; i++) {
        if (s_issues[i].severity == VHMS_SEV_CRITICAL && !s_issues[i].acknowledged)
            n++;
    }
    return n;
}

size_t fleet_open_issue_count(size_t vehicle_idx)
{
    size_t n = 0, i;

    for (i = 0; i < s_issue_count; i++) {
        if ((size_t)s_issues[i].vehicle_idx == vehicle_idx)
            n++;
    }
    return n;
}

void fleet_toggle_acknowledged(const vhms_issue_t *issue)
{
    size_t i;

    if (!issue)
        return;
    for (i = 0; i < s_issue_count; i++) {
        if (&s_issues[i] == issue) {
            s_issues[i].acknowledged = !s_issues[i].acknowledged;
            return;
        }
    }
}

const char *vhms_severity_name(vhms_severity_t s)
{
    switch (s) {
    case VHMS_SEV_OK:       return "OK";
    case VHMS_SEV_ADVISORY:  return "ADVISORY";
    case VHMS_SEV_CAUTION:   return "CAUTION";
    case VHMS_SEV_CRITICAL:  return "CRITICAL";
    default:                 return "?";
    }
}
