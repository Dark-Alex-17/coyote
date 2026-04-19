# Test Plan: Vault

## Behaviors to test
- [ ] Vault add stores encrypted secret
- [ ] Vault get decrypts and returns secret
- [ ] Vault update replaces secret value
- [ ] Vault delete removes secret
- [ ] Vault list shows all secret names
- [ ] Secrets interpolated in MCP config (mcp.json)
- [ ] Missing secrets produce warning during MCP init
- [ ] Vault accessible from REPL (.vault commands)
- [ ] Vault accessible from CLI (--add/get/update/delete-secret)

## Old code reference
- `src/vault/mod.rs` — GlobalVault, operations
- `src/mcp/mod.rs` — interpolate_secrets
