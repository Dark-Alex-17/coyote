fn _loki_elvish {
    var line = (edit:current-command)
    var new-line = (loki -e $line)
    edit:replace-input $new-line
}

edit:insert:binding[Alt-e] = $_loki_elvish