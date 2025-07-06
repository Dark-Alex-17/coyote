function _loki_fish
    set -l _old (commandline)
    if test -n $_old
        echo -n "⌛"
        commandline -f repaint
        commandline (loki -e $_old)
    end
end
bind \ee _loki_fish