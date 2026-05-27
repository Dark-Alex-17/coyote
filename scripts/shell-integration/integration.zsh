_coyote_zsh() {
    if [[ -n "$BUFFER" ]]; then
        local _old=$BUFFER
        BUFFER+="⌛"
        zle -I && zle redisplay
        BUFFER=$(coyote -e "$_old")
        zle end-of-line
    fi
}
zle -N _coyote_zsh
bindkey '\ee' _coyote_zsh