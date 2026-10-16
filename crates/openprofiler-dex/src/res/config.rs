//! `ResTable_config` → resource folder qualifiers (`-de-rDE`, `-night`, `-xxhdpi`, `-v21`).

use super::reader::{slice_at, u8_at, u16_at};

/// Qualifier suffix for a values folder, in aapt order, each part with a leading dash.
/// Fields beyond the config's declared size read as zero (unset).
pub(crate) fn qualifiers(c: &[u8]) -> String {
    let b = |off| u8_at(c, off).unwrap_or(0);
    let h = |off| u16_at(c, off).unwrap_or(0);
    let mut q: Vec<String> = Vec::new();

    let (mcc, mnc) = (h(4), h(6));
    if mcc != 0 {
        q.push(format!("mcc{mcc}"));
    }
    if mnc != 0 {
        // MNC_ZERO is stored as 0xffff.
        q.push(if mnc == 0xffff { "mnc00".into() } else { format!("mnc{mnc:02}") });
    }

    let lang = unpack_locale(slice_at(c, 8, 2), b'a');
    let region = unpack_locale(slice_at(c, 10, 2), b'0');
    let script = ascii_field(slice_at(c, 36, 4));
    let variant = ascii_field(slice_at(c, 40, 8));
    if !script.is_empty() || !variant.is_empty() {
        if !lang.is_empty() {
            let mut s = format!("b+{lang}");
            for part in [&script, &region, &variant] {
                if !part.is_empty() {
                    s.push('+');
                    s.push_str(part);
                }
            }
            q.push(s);
        }
    } else if !lang.is_empty() {
        q.push(if region.is_empty() { lang } else { format!("{lang}-r{region}") });
    }

    let screen_layout = b(28);
    match screen_layout & 0xc0 {
        0x40 => q.push("ldltr".into()),
        0x80 => q.push("ldrtl".into()),
        _ => {}
    }
    let sw = h(30);
    if sw != 0 {
        q.push(format!("sw{sw}dp"));
    }
    let (w, ht) = (h(32), h(34));
    if w != 0 {
        q.push(format!("w{w}dp"));
    }
    if ht != 0 {
        q.push(format!("h{ht}dp"));
    }
    push_named(&mut q, screen_layout & 0x0f, &[(1, "small"), (2, "normal"), (3, "large"), (4, "xlarge")]);
    push_named(&mut q, screen_layout & 0x30, &[(0x10, "notlong"), (0x20, "long")]);
    push_named(&mut q, b(48) & 0x03, &[(1, "notround"), (2, "round")]);
    let color_mode = b(49);
    push_named(&mut q, color_mode & 0x03, &[(1, "nowidecg"), (2, "widecg")]);
    push_named(&mut q, color_mode & 0x0c, &[(4, "lowdr"), (8, "highdr")]);
    push_named(&mut q, b(12), &[(1, "port"), (2, "land"), (3, "square")]);
    let ui_mode = b(29);
    push_named(
        &mut q,
        ui_mode & 0x0f,
        &[(2, "desk"), (3, "car"), (4, "television"), (5, "appliance"), (6, "watch"), (7, "vrheadset")],
    );
    push_named(&mut q, ui_mode & 0x30, &[(0x10, "notnight"), (0x20, "night")]);
    match h(14) {
        0 => {}
        120 => q.push("ldpi".into()),
        160 => q.push("mdpi".into()),
        213 => q.push("tvdpi".into()),
        240 => q.push("hdpi".into()),
        320 => q.push("xhdpi".into()),
        480 => q.push("xxhdpi".into()),
        640 => q.push("xxxhdpi".into()),
        0xfffe => q.push("anydpi".into()),
        0xffff => q.push("nodpi".into()),
        d => q.push(format!("{d}dpi")),
    }
    push_named(&mut q, b(13), &[(1, "notouch"), (2, "stylus"), (3, "finger")]);
    let input = b(18);
    push_named(&mut q, input & 0x03, &[(1, "keysexposed"), (2, "keyshidden"), (3, "keyssoft")]);
    push_named(&mut q, b(16), &[(1, "nokeys"), (2, "qwerty"), (3, "12key")]);
    push_named(&mut q, input & 0x0c, &[(4, "navexposed"), (8, "navhidden")]);
    push_named(&mut q, b(17), &[(1, "nonav"), (2, "dpad"), (3, "trackball"), (4, "wheel")]);
    let (sw_px, sh_px) = (h(20), h(22));
    if sw_px != 0 && sh_px != 0 {
        q.push(format!("{sw_px}x{sh_px}"));
    }
    let sdk = h(24);
    if sdk != 0 {
        q.push(format!("v{sdk}"));
    }

    q.iter().map(|p| format!("-{p}")).collect()
}

fn push_named(q: &mut Vec<String>, value: u8, names: &[(u8, &str)]) {
    if let Some((_, n)) = names.iter().find(|(v, _)| *v == value) {
        q.push((*n).to_string());
    }
}

/// Unpacks a 2-byte language/region code; 3-letter codes are packed into 15 bits.
fn unpack_locale(b: Option<&[u8]>, base: u8) -> String {
    let Some(&[b0, b1]) = b else {
        return String::new();
    };
    if b0 == 0 {
        return String::new();
    }
    if b0 & 0x80 != 0 {
        let chars = [b1 & 0x1f, ((b1 & 0xe0) >> 5) | ((b0 & 0x03) << 3), (b0 & 0x7c) >> 2];
        return chars.iter().map(|&c| char::from(base.wrapping_add(c))).collect();
    }
    ascii_field(Some(&[b0, b1]))
}

/// A NUL-padded ASCII field; non-alphanumeric content is treated as unset.
fn ascii_field(b: Option<&[u8]>) -> String {
    let s: String = b.unwrap_or_default().iter().take_while(|&&c| c != 0).map(|&c| char::from(c)).collect();
    if s.chars().all(|c| c.is_ascii_alphanumeric()) { s } else { String::new() }
}
