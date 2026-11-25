# Shared board selection for the shell recipes.
malak_exe=${MALAK_RECIPE_EXE:-malak}

question_body() {
    printf '%s\n' "$1"
    if [ -n "$2" ]; then printf 'Blocks: %s\n' "$2"; fi
    if [ -n "$3" ]; then printf 'Options:\n%s' "$3"; fi
    if [ -n "$4" ]; then printf 'Recommendation: %s\n' "$4"; fi
}

task_send() {
    if [ -n "${MALAK_TASK_TRACE-}" ]; then
        set -- --trace "$MALAK_TASK_TRACE" "$@"
    fi
    if [ -n "${MALAK_TASK_CREATION-}" ]; then
        set -- --reply-to "$MALAK_TASK_INBOX" "$MALAK_TASK_CREATION" "$MALAK_TASK_POSITION" "$@"
    fi
    set -- send "$@"
    if [ -n "${MALAK_RECIPE_CONFIG-}" ]; then
        set -- --config "$MALAK_RECIPE_CONFIG" "$@"
    fi
    "$malak_exe" "$@"
}

recipe_config() {
    if [ -n "$1" ] && [ -n "${MALAK_RECIPE_CONFIG-}" ]; then
        recipe_notice 'select the board' 'A board was chosen twice.' \
            'Use one --board or --config option.' \
            'Choose a board only once. Use one --board or --config option.' >&2
        return 2
    fi
    printf '%s\n' "${1:-${MALAK_RECIPE_CONFIG-}}"
}

recipe_config_option() {
    recipe_config "$2" >/dev/null || return
    case $1 in
        --board) board_config "$2" ;;
        --config) printf '%s\n' "$2" ;;
    esac
}

check_recipe_board() {
    if [ -n "${MALAK_HANDOVER-}" ]; then
        "$malak_exe" --config "$1" recipe __check-board
    fi
}

board_config() {
    if [ "$#" -eq 0 ]; then
        if [ -n "${MALAK_RECIPE_CONFIG-}" ]; then
            printf '%s\n' "$MALAK_RECIPE_CONFIG"
            return
        fi
        case ${XDG_CONFIG_HOME-} in
            /*) printf '%s/malak/board.toml\n' "$XDG_CONFIG_HOME" ;;
            *) printf '%s/.config/malak/board.toml\n' "$HOME" ;;
        esac
        return
    fi
    "$malak_exe" recipe __board-config "$@"
}

# The caller sets recipes to this directory before sourcing board.sh.
recipe_output() {
    python3 "$recipes/recipe_output.py" "$@"
}

recipe_usage() {
    if [ -t 1 ] && [ "${TERM-}" != dumb ] && command -v python3 >/dev/null 2>&1; then
        recipe_output usage "$@"
    else
        mode=$1
        shift
        if [ "$mode" = help ] && [ -n "${MALAK_RECIPE_DESCRIPTION-}" ]; then
            printf '%s\n\n' "$MALAK_RECIPE_DESCRIPTION"
        fi
        printf '%s\n' "$@"
        if [ "$mode" = help ] && [ -n "${MALAK_RECIPE_EXAMPLE-}" ]; then
            printf '\nExample:\n%s\n' "$MALAK_RECIPE_EXAMPLE"
        fi
    fi
}

recipe_notice() {
    if [ -t 1 ] && [ "${TERM-}" != dumb ] && command -v python3 >/dev/null 2>&1; then
        recipe_output notice "$1" "$2" "$3"
    else
        printf '%s\n' "$4"
    fi
}
