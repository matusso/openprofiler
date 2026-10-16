//! Android resources: binary XML (`AndroidManifest.xml`, `res/**/*.xml`) and `resources.arsc`.
//!
//! Everything here parses untrusted, often deliberately malformed input: all reads are
//! bounds-checked, bad chunks are skipped and nothing panics.

mod arsc;
mod attrs;
mod axml;
mod config;
mod reader;
mod strings;
mod value;
mod values;

#[cfg(test)]
mod tests;

pub use arsc::ResTable;

/// Decodes Android binary XML (AXML) to pretty-printed text XML. `table` (optional) resolves
/// resource references (`@0x7f010000` → `@string/app_name`).
pub fn decode_xml(data: &[u8], table: Option<&ResTable>) -> anyhow::Result<String> {
    axml::decode_xml(data, table)
}

/// True when `data` starts with a binary XML chunk header.
pub fn is_binary_xml(data: &[u8]) -> bool {
    axml::is_binary_xml(data)
}
