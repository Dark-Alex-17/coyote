fn _coyote_elvish {
    var line = (edit:current-command)
    var new-line = (coyote -e $line)
    edit:replace-input $new-line
}

edit:insert:binding[Alt-e] = $_coyote_elvish