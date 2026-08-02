/*
 * BakBeat NetMD WinUSB installer
 *
 * Copyright (c) 2026 BakBeat contributors
 *
 * This file is free software; you can redistribute it and/or modify it under
 * the terms of the GNU Lesser General Public License as published by the Free
 * Software Foundation; either version 3 of the License, or (at your option)
 * any later version.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "libwdi.h"

#define BAKBEAT_DRIVER_GUID "{9E3F7C2A-27B7-4E73-A91E-AC4A7AF70B8B}"
#define BAKBEAT_DRIVER_DIR "bakbeat-netmd-driver"
#define BAKBEAT_INF_NAME "bakbeat-netmd-winusb.inf"

static void result(const char* outcome, int code, unsigned short vid,
    unsigned short pid)
{
    printf("{\"schemaVersion\":1,\"outcome\":\"%s\",\"nativeCode\":%d,"
        "\"vendorId\":\"0x%04x\",\"productId\":\"0x%04x\","
        "\"driver\":\"WinUSB\"}\n",
        outcome, code, vid, pid);
}

static int parse_hex16(const char* value, unsigned short* parsed)
{
    char* end = NULL;
    unsigned long number;
    if (value == NULL || parsed == NULL)
        return 0;
    number = strtoul(value, &end, 0);
    if (end == value || *end != '\0' || number == 0 || number > 0xffff)
        return 0;
    *parsed = (unsigned short)number;
    return 1;
}

int __cdecl main(int argc, char** argv)
{
    struct wdi_device_info* list = NULL;
    struct wdi_device_info* item;
    struct wdi_device_info selected;
    struct wdi_options_create_list list_options = { 0 };
    struct wdi_options_prepare_driver prepare_options = { 0 };
    struct wdi_options_install_driver install_options = { 0 };
    unsigned short vid = 0, pid = 0;
    const char* requested_device_id = NULL;
    int matches = 0;
    int code, i;

    if (argc < 8 || strcmp(argv[1], "install") != 0) {
        result("invalidArguments", WDI_ERROR_INVALID_PARAM, vid, pid);
        return 1;
    }
    for (i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--vid") == 0 && ++i < argc) {
            if (!parse_hex16(argv[i], &vid)) goto invalid;
        } else if (strcmp(argv[i], "--pid") == 0 && ++i < argc) {
            if (!parse_hex16(argv[i], &pid)) goto invalid;
        } else if (strcmp(argv[i], "--device-id") == 0 && ++i < argc) {
            requested_device_id = argv[i];
        } else if (strcmp(argv[i], "--json") != 0) {
            goto invalid;
        }
    }
    if (vid == 0 || pid == 0 || requested_device_id == NULL ||
        requested_device_id[0] == '\0') goto invalid;

    wdi_set_log_level(WDI_LOG_LEVEL_NONE);
    list_options.list_all = TRUE;
    list_options.list_hubs = FALSE;
    list_options.trim_whitespaces = TRUE;
    code = wdi_create_list(&list, &list_options);
    if (code != WDI_SUCCESS) {
        result("enumerationFailed", code, vid, pid);
        return 4;
    }

    memset(&selected, 0, sizeof(selected));
    for (item = list; item != NULL; item = item->next) {
        if (item->vid != vid || item->pid != pid || item->is_composite ||
            item->device_id == NULL ||
            _stricmp(item->device_id, requested_device_id) != 0)
            continue;
        selected = *item;
        matches++;
    }
    if (matches != 1) {
        wdi_destroy_list(list);
        result(matches == 0 ? "noDevice" : "ambiguousDevice",
            WDI_ERROR_NO_DEVICE, vid, pid);
        return 2;
    }

    prepare_options.driver_type = WDI_WINUSB;
    prepare_options.vendor_name = "BakBeat";
    prepare_options.device_guid = BAKBEAT_DRIVER_GUID;
    prepare_options.cert_subject = "CN=BakBeat NetMD WinUSB";
    code = wdi_prepare_driver(&selected, BAKBEAT_DRIVER_DIR,
        BAKBEAT_INF_NAME, &prepare_options);
    if (code != WDI_SUCCESS) {
        wdi_destroy_list(list);
        result(code == WDI_ERROR_UNSIGNED ? "signingBlocked" :
            "prepareFailed", code, vid, pid);
        return 4;
    }

    install_options.pending_install_timeout = 120000;
    code = wdi_install_driver(&selected, BAKBEAT_DRIVER_DIR,
        BAKBEAT_INF_NAME, &install_options);
    wdi_destroy_list(list);
    if (code != WDI_SUCCESS) {
        result(code == WDI_ERROR_NEEDS_ADMIN ? "elevationRequired" :
            code == WDI_ERROR_USER_CANCEL ? "cancelled" :
            code == WDI_ERROR_UNSIGNED ? "signingBlocked" :
            "installFailed", code, vid, pid);
        return code == WDI_ERROR_USER_CANCEL ? 3 : 4;
    }

    result("installed", WDI_SUCCESS, vid, pid);
    return 0;

invalid:
    result("invalidArguments", WDI_ERROR_INVALID_PARAM, vid, pid);
    return 1;
}
