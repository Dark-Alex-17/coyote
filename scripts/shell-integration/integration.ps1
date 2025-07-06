Set-PSReadLineKeyHandler -Chord "alt+e" -ScriptBlock {
    $_old = $null
    [Microsoft.PowerShell.PSConsoleReadline]::GetBufferState([ref]$_old, [ref]$null)
    if ($_old) {
        [Microsoft.PowerShell.PSConsoleReadLine]::Insert('⌛')
        $_new = (loki -e $_old)
        [Microsoft.PowerShell.PSConsoleReadLine]::DeleteLine()
        [Microsoft.PowerShell.PSConsoleReadline]::Insert($_new)
    }
}