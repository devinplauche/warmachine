/*
 * data_model.h - fleet / vehicle / maintenance-issue data model.
 *
 * THE SEAM: everything the UI renders comes from the static tables filled
 * by fleet_data_load().  To wire a real VHMS feed, replace the body of
 * fleet_data_load() with the vehicle-bus / telematics ingestion path and
 * keep the read API below unchanged - no screen code touches the tables
 * directly.
 */
#ifndef VHMS_DATA_MODEL_H
#define VHMS_DATA_MODEL_H

#include <stddef.h>

#define VHMS_ID_LEN     16
#define VHMS_NAME_LEN   48
#define VHMS_DTC_LEN    8
#define VHMS_TITLE_LEN  64
#define VHMS_SYSTEM_LEN 32
#define VHMS_DESC_LEN   256
#define VHMS_ACTION_LEN 256
#define VHMS_PART_LEN   48
#define VHMS_MAX_PARTS  4

#define VHMS_MAX_VEHICLES 8
#define VHMS_MAX_ISSUES   24

typedef enum {
    VHMS_SEV_OK = 0,
    VHMS_SEV_ADVISORY,
    VHMS_SEV_CAUTION,
    VHMS_SEV_CRITICAL
} vhms_severity_t;

typedef struct {
    char id[VHMS_ID_LEN];
    char name[VHMS_NAME_LEN];
    unsigned int odometer_km;
    /* Worst severity across ALL issues on the vehicle (acknowledged or not:
     * an acknowledged fault is still a fault). */
    vhms_severity_t status;
} vhms_vehicle_t;

typedef struct {
    int vehicle_idx; /* index into the vehicle table */
    char dtc[VHMS_DTC_LEN];
    char title[VHMS_TITLE_LEN];
    char system[VHMS_SYSTEM_LEN]; /* powertrain / chassis / electrical / body */
    vhms_severity_t severity;
    char description[VHMS_DESC_LEN];
    char recommended_action[VHMS_ACTION_LEN];
    char parts[VHMS_MAX_PARTS][VHMS_PART_LEN];
    unsigned int parts_count;
    int acknowledged; /* boolean */
} vhms_issue_t;

/* Fill the static tables (currently: built-in demo fleet). */
void fleet_data_load(void);

size_t fleet_vehicle_count(void);
const vhms_vehicle_t *fleet_vehicle_at(size_t i);

size_t fleet_issue_count(void);
const vhms_issue_t *fleet_issue_at(size_t i);

/* Issue pointers for one vehicle; returns the count (<= out_cap). */
size_t fleet_issues_for_vehicle(size_t vehicle_idx,
                                const vhms_issue_t **out, size_t out_cap);

/* Unacknowledged CRITICAL issues across the whole fleet. */
size_t fleet_critical_count(void);

/* All issues (acknowledged or not) on one vehicle. */
size_t fleet_open_issue_count(size_t vehicle_idx);

const char *vhms_severity_name(vhms_severity_t s);

/* Flip the acknowledged flag on one issue (located by pointer). */
void fleet_toggle_acknowledged(const vhms_issue_t *issue);

#endif /* VHMS_DATA_MODEL_H */
