// Each integration test binary compiles this module on its own, and most
// of them need only one of the helpers.
#[allow(dead_code)]
pub mod fake_llm;
#[allow(dead_code)]
pub mod pty;
