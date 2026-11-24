#!/bin/sh
# The worker command writes only its final result to stdout.
set -eu
umask 077
recipes=$(CDPATH='' cd "$(dirname "$0")" && pwd)
. "$recipes/board.sh"
case ${1-} in
    -h|--help)
        recipe_usage help 'Usage: malak recipe wrapper [--board NAME | --config PATH] [--] CMD…' \
            'Set MALAK_WORKER_HANDOVER, MALAK_REPORT_ID and MALAK_WRAPPER_STATE.'
        exit 0 ;;
    --board|--config)
        [ "$#" -ge 2 ] && [ -n "$2" ] || exit 2
        MALAK_RECIPE_CONFIG=$(recipe_config_option "$1" "$2")
        export MALAK_RECIPE_CONFIG
        shift 2
        case ${1-} in --board|--config) recipe_config_option "$1" "${2-}" >/dev/null; exit 2 ;; esac ;;
esac
if [ "${1-}" = -- ]; then shift; fi
if [ -z "${MALAK_WORKER_HANDOVER-}" ] || [ -z "${MALAK_REPORT_ID-}" ] || [ -z "${MALAK_WRAPPER_STATE-}" ]; then
    recipe_output error "start the worker" "Worker wrapper environment is incomplete." \
        "Set MALAK_WORKER_HANDOVER, MALAK_REPORT_ID and MALAK_WRAPPER_STATE."
    exit 2
fi
if [ "$#" -eq 0 ]; then
    recipe_output error "start the worker" "No worker command was given." "Pass the worker command to malak recipe wrapper."
    exit 2
fi

if [ -n "${MALAK_RECIPE_CONFIG-}" ]; then
    MALAK_HANDOVER="$MALAK_WORKER_HANDOVER" check_recipe_board "$MALAK_RECIPE_CONFIG"
fi
run=$(mktemp -d "${MALAK_WRAPPER_STATE%/}/result.XXXXXXXX")
status=0
"$@" > "$run/out" 2> "$run/err" || status=$?
report=$recipes/report.sh
if "$report" "$status" "$run/out"; then
    exit "$status"
else
    sent=$?
    recipe_output diagnostic "Error: Could not send the worker report to lead." \
        Result "$run/out" "Worker exit" "$status" "Report ID" "$MALAK_REPORT_ID" \
        Next "Retry the report with the same ID."
    exit "$sent"
fi
