def _loki_nushell [] {
    let _prev = (commandline)
    if ($_prev != "") {
        print '⌛'
        commandline edit -r (loki -e $_prev)
    }
}

$env.config.keybindings = ($env.config.keybindings | append {
        name: loki_integration
        modifier: alt
        keycode: char_e
        mode: [emacs, vi_insert]
        event:[
            {
                send: executehostcommand,
                cmd: "_loki_nushell"
            }
        ]
    }
)