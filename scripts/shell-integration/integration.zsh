_loki_zsh() {
    if [[ -n "$BUFFER" ]]; then
        local _old=$BUFFER
        BUFFER+="⌛"
        zle -I && zle redisplay
        BUFFER=$(loki -e "$_old")
        zle end-of-line
    fi
}
zle -N _loki_zsh
bindkey '\ee' _loki_zsh