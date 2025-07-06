_loki_bash() {
    if [[ -n "$READLINE_LINE" ]]; then
        READLINE_LINE=$(loki -e "$READLINE_LINE")
        READLINE_POINT=${#READLINE_LINE}
    fi
}
bind -x '"\ee": _loki_bash'