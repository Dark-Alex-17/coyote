_coyote_bash() {
    if [[ -n "$READLINE_LINE" ]]; then
        READLINE_LINE=$(coyote -e "$READLINE_LINE")
        READLINE_POINT=${#READLINE_LINE}
    fi
}
bind -x '"\ee": _coyote_bash'