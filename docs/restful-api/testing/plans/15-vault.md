# Test Plan: Vault

## Behaviors to test
- [ ] Vault add stores encrypted secret (requires terminal + password file)
- [ ] Vault get decrypts and returns secret (requires password file)
- [ ] Vault update replaces secret value (requires terminal + password file)
- [ ] Vault delete removes secret (requires password file)
- [ ] Vault list shows all secret names (requires password file)
- [ ] Secrets interpolated in MCP config (mcp.json) (requires Vault with secrets)
- [ ] Missing secrets produce warning during MCP init (requires Vault)
- [x] Vault accessible from CLI (flag parsing tested in iteration 10)
- [ ] Vault accessible from REPL (.vault commands) (requires REPL infra)

## Additional behaviors tested

- [x] SECRET_RE matches {{DOUBLE_BRACES}}
- [x] SECRET_RE matches with surrounding text
- [x] SECRET_RE does not match {SINGLE_BRACES}
- [x] SECRET_RE does not match plain text
- [x] SECRET_RE matches with spaces inside braces
- [x] Vault::default() creates instance with no password file

## Old code reference
- `src/vault/mod.rs` — GlobalVault, operations
- `src/mcp/mod.rs` — interpolate_secrets
