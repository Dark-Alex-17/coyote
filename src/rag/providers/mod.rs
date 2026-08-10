mod yaml;
// Use `self::` on every re-export in this file. Once a `mod duckdb;` sits here
// alongside a dependency on the `duckdb` CRATE, a bare `pub use duckdb::...`
// is ambiguous (E0659) — `use` paths resolve against both this module's items
// and the extern prelude, and `use` declarations may not shadow.
pub use self::yaml::YamlProvider;
