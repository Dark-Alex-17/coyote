function _coyote_fish
    set -l _old (commandline)
    if test -n $_old
        echo -n "⌛"
        commandline -f repaint
        commandline (coyote -e $_old)
    end
end
bind \ee _coyote_fish