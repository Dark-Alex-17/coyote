def _coyote_nushell [] {
    let _prev = (commandline)
    if ($_prev != "") {
        print '⌛'
        commandline edit -r (coyote -e $_prev)
    }
}

$env.config.keybindings = ($env.config.keybindings | append {
        name: coyote_integration
        modifier: alt
        keycode: char_e
        mode: [emacs, vi_insert]
        event:[
            {
                send: executehostcommand,
                cmd: "_coyote_nushell"
            }
        ]
    }
)