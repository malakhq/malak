#!/bin/sh
# Called by the starter, with the exit status and its captured stdout.
set -eu
recipes=$(CDPATH='' cd "$(dirname "$0")" && pwd)
. "$recipes/board.sh"
if [ -z "${MALAK_WORKER_HANDOVER-}" ] || [ -z "${MALAK_REPORT_ID-}" ] || [ "$#" -ne 2 ]; then
    recipe_output error "send the worker report" "Report inputs are incomplete." \
        "Set MALAK_WORKER_HANDOVER and MALAK_REPORT_ID, then pass the worker exit status and result path."
    exit 2
fi
export MALAK_HANDOVER="$MALAK_WORKER_HANDOVER"
send_report() {
    if [ "${MALAK_REPORT_JSON-}" = 1 ]; then
        set -- --json "$@"
    fi
    task_send --kind report --id "$MALAK_REPORT_ID" "$@"
}
if [ -s "$2" ]; then
    header="Worker exit status: $1"
    {
        printf '%s\n\n' "$header"
        cat < "$2"
    } | send_report --max-bytes "$((262144 + ${#header} + 2))" lead -
else
    send_report lead "Worker exited without a result (exit status $1)."
fi
