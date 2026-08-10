mod yaml;
// Use `self::` on every re-export in this file. Once a `mod duckdb;` sits here
// alongside a dependency on the `duckdb` CRATE, a bare `pub use duckdb::...`
// is ambiguous (E0659) — `use` paths resolve against both this module's items
// and the extern prelude, and `use` declarations may not shadow.
pub use self::yaml::YamlProvider;

mod duckdb;
pub use self::duckdb::DuckDbProvider;
// `create()` in rag/mod.rs derives the sidecar path through this. It is `pub(crate)`
// in providers/duckdb.rs, and the re-export must be `pub(crate)` too — a `pub use` of
// a `pub(crate)` item is E0364/E0365.
pub(crate) use self::duckdb::duckdb_path_from_yaml;
