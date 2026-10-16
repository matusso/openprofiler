//! `Res_value` typed values and their textual forms.

use super::ResTable;
use super::reader::{u8_at, u32_at};
use super::strings::StringPool;

pub(crate) const TYPE_NULL: u8 = 0x00;
pub(crate) const TYPE_REFERENCE: u8 = 0x01;
pub(crate) const TYPE_ATTRIBUTE: u8 = 0x02;
pub(crate) const TYPE_STRING: u8 = 0x03;
const TYPE_FLOAT: u8 = 0x04;
const TYPE_DIMENSION: u8 = 0x05;
const TYPE_FRACTION: u8 = 0x06;
const TYPE_DYNAMIC_REFERENCE: u8 = 0x07;
const TYPE_DYNAMIC_ATTRIBUTE: u8 = 0x08;
pub(crate) const TYPE_INT_DEC: u8 = 0x10;
pub(crate) const TYPE_INT_HEX: u8 = 0x11;
pub(crate) const TYPE_INT_BOOLEAN: u8 = 0x12;
const TYPE_INT_COLOR_ARGB8: u8 = 0x1c;
const TYPE_INT_COLOR_RGB8: u8 = 0x1d;
const TYPE_INT_COLOR_ARGB4: u8 = 0x1e;
const TYPE_INT_COLOR_RGB4: u8 = 0x1f;

/// `Res_value`: a data type and 32 bits of data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResValue {
    pub typ: u8,
    pub data: u32,
}

impl ResValue {
    /// Reads the 8-byte `Res_value` at `off` (size u16, res0 u8, dataType u8, data u32).
    pub fn read(d: &[u8], off: usize) -> Option<ResValue> {
        Some(ResValue { typ: u8_at(d, off.checked_add(3)?)?, data: u32_at(d, off.checked_add(4)?)? })
    }

    pub fn is_string(self) -> bool {
        self.typ == TYPE_STRING
    }

    /// Unescaped textual form, e.g. `@string/app_name`, `16dp`, `#ff112233`, or the string.
    pub fn format(self, strings: &StringPool, table: Option<&ResTable>) -> String {
        let d = self.data;
        match self.typ {
            TYPE_NULL if d == 1 => String::new(),
            TYPE_NULL => "@null".into(),
            TYPE_REFERENCE | TYPE_DYNAMIC_REFERENCE => format_reference(d, '@', table),
            TYPE_ATTRIBUTE | TYPE_DYNAMIC_ATTRIBUTE => format_reference(d, '?', table),
            TYPE_STRING => strings.get_or_empty(d),
            TYPE_FLOAT => format_float(f32::from_bits(d)),
            TYPE_DIMENSION => {
                const UNITS: [&str; 6] = ["px", "dp", "sp", "pt", "in", "mm"];
                let unit = UNITS.get((d & 0xf) as usize).copied().unwrap_or("");
                format!("{}{unit}", format_float(complex_value(d)))
            }
            TYPE_FRACTION => {
                let unit = if d & 0xf == 1 { "%p" } else { "%" };
                format!("{}{unit}", format_float(complex_value(d) * 100.0))
            }
            TYPE_INT_HEX => format!("0x{d:08x}"),
            TYPE_INT_BOOLEAN => (if d == 0 { "false" } else { "true" }).into(),
            TYPE_INT_COLOR_ARGB8 => format!("#{d:08x}"),
            TYPE_INT_COLOR_RGB8 => format!("#{:06x}", d & 0xff_ffff),
            TYPE_INT_COLOR_ARGB4 => {
                format!("#{:x}{:x}{:x}{:x}", d >> 28, (d >> 20) & 0xf, (d >> 12) & 0xf, (d >> 4) & 0xf)
            }
            TYPE_INT_COLOR_RGB4 => {
                format!("#{:x}{:x}{:x}", (d >> 20) & 0xf, (d >> 12) & 0xf, (d >> 4) & 0xf)
            }
            TYPE_INT_DEC => (d as i32).to_string(),
            _ => format!("0x{d:08x}"),
        }
    }
}

fn format_reference(id: u32, sigil: char, table: Option<&ResTable>) -> String {
    if id == 0 {
        return if sigil == '@' { "@null".into() } else { "?0x00000000".into() };
    }
    match table.and_then(|t| t.name_of(id)) {
        Some(name) => format!("{sigil}{}", name.trim_start_matches('@')),
        None => format!("{sigil}0x{id:08x}"),
    }
}

/// Decodes a complex (dimension/fraction) value: 24-bit signed mantissa with a radix.
fn complex_value(d: u32) -> f32 {
    const RADIX: [f32; 4] = [
        1.0 / (1u32 << 8) as f32,
        1.0 / (1u32 << 15) as f32,
        1.0 / (1u32 << 23) as f32,
        1.0 / (1u32 << 31) as f32,
    ];
    let mantissa = (d & 0xffff_ff00) as i32 as f32;
    mantissa * RADIX[((d >> 4) & 3) as usize]
}

/// Shortest round-trip form: `16`, `1.5`, `0.33333334`.
fn format_float(f: f32) -> String {
    let s = f.to_string();
    if f.is_finite() && s.len() > 12 {
        // Drop float noise from radix conversion, e.g. 0.30000001 -> 0.3.
        let short = format!("{f:.6}");
        let short = short.trim_end_matches('0').trim_end_matches('.');
        if short.parse::<f32>().is_ok_and(|v| (v - f).abs() <= f.abs() * 1e-6) {
            return short.to_string();
        }
    }
    s
}

/// Escapes text for XML content and attribute values.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("&#{};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Escapes a string resource the way aapt parses `res/values` (quotes, backslashes,
/// newlines, a leading `@`/`?`), then for XML text.
pub(crate) fn aapt_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    // aapt collapses unquoted whitespace, so keep such strings verbatim inside quotes.
    let quote = s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) || s.contains("  ");
    if quote {
        out.push('"');
    } else if s.starts_with('@') || s.starts_with('?') {
        out.push('\\');
    }
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    if quote {
        out.push('"');
    }
    out
}
