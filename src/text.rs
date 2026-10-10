//! Terminal-safe text and unit formatting shared by the UI, `--once` and tests.

/// Make kernel- or user-controlled bytes (file names, mount points, command
/// lines, sysfs strings) safe for a terminal while keeping them distinguishable.
///
/// Valid UTF-8 text is kept, except that control characters, bidirectional
/// overrides and other invisible format characters are escaped as `\xNN` or
/// `\u{NNNN}`. Bytes that are not UTF-8 become `\xNN`. A backslash is doubled,
/// so distinct names never collapse into the same display string.
pub fn sanitize_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                push_escaped(&mut out, valid);
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                // SAFETY-free: `valid_up_to` guarantees this prefix is UTF-8.
                push_escaped(&mut out, std::str::from_utf8(valid).unwrap_or_default());
                let invalid = error.error_len().unwrap_or(after.len()).max(1);
                for byte in &after[..invalid.min(after.len())] {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
                rest = &after[invalid.min(after.len())..];
            }
        }
    }
    out
}

pub fn sanitize(text: &str) -> String {
    sanitize_bytes(text.as_bytes())
}

fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if (c as u32) < 0x80 && c.is_control() {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else if c.is_control() || is_invisible_format(c) {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
}

/// Characters that can reorder or hide terminal text (Trojan Source style).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c as u32,
        0x061c | 0x200b..=0x200f | 0x2028..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff
    )
}

/// Byte quantities use binary units, as `df -h` and `du -h` do.
pub fn format_bytes(bytes: f64) -> String {
    let (value, unit, decimals) = scaled(
        bytes.max(0.0),
        1024.0,
        &["B", "KiB", "MiB", "GiB", "TiB", "PiB"],
        |index| if index == 0 { 0 } else { 1 },
    );
    format!("{value:.decimals$} {unit}")
}

/// htop-style compact bytes for meters: "512B", "12.3M", "160M", "1.4T".
pub fn format_bytes_short(bytes: f64) -> String {
    let units = ["B", "K", "M", "G", "T", "P"];
    let mut value = if bytes.is_finite() {
        bytes.max(0.0)
    } else {
        0.0
    };
    let mut index = 0;
    loop {
        let decimals = usize::from(index > 0 && value < 99.95);
        let factor = 10f64.powi(decimals as i32);
        let rounded = (value * factor).round() / factor;
        if rounded < 1023.5 || index + 1 == units.len() {
            return format!("{rounded:.decimals$}{}", units[index]);
        }
        value /= 1024.0;
        index += 1;
    }
}

/// At most four characters for one-row meters: "5.9M", "80M", "512B".
pub fn format_bytes_tiny(bytes: f64) -> String {
    let units = ["B", "K", "M", "G", "T", "P"];
    let mut value = if bytes.is_finite() {
        bytes.max(0.0)
    } else {
        0.0
    };
    let mut index = 0;
    loop {
        let decimals = usize::from(index > 0 && value < 9.95);
        let factor = 10f64.powi(decimals as i32);
        let rounded = (value * factor).round() / factor;
        // Three digits at most, so 1000-1023 of a unit move to the next one.
        if rounded < 999.5 || index + 1 == units.len() {
            return format!("{rounded:.decimals$}{}", units[index]);
        }
        value /= 1024.0;
        index += 1;
    }
}

pub fn format_rate(bytes: f64) -> String {
    format!("{}/s", format_bytes(bytes))
}

/// A signed size difference, for example "+1.2 GiB", "-512 B" or "0 B".
pub fn format_delta(delta: i64) -> String {
    let text = format_bytes(delta.unsigned_abs() as f64);
    match delta.cmp(&0) {
        std::cmp::Ordering::Greater => format!("+{text}"),
        std::cmp::Ordering::Less => format!("-{text}"),
        std::cmp::Ordering::Equal => text,
    }
}

/// Group thousands for counts, for example 1234567 -> "1,234,567".
pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Compact count for narrow columns: 999, 1.2k, 12k, 1.2M.
pub fn format_short_count(count: f64) -> String {
    let count = if count.is_finite() {
        count.max(0.0)
    } else {
        0.0
    };
    let (value, unit, decimals) = scaled(count, 1000.0, &["", "k", "M", "G", "T"], |index| {
        if index == 0 { 0 } else { 1 }
    });
    format!("{value:.decimals$}{unit}")
}

/// Human duration for scan ages and durations: "45s", "3m", "2h 5m", "3d".
pub fn format_duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => {
            let minutes = (seconds % 3600) / 60;
            if minutes == 0 {
                format!("{}h", seconds / 3600)
            } else {
                format!("{}h {minutes}m", seconds / 3600)
            }
        }
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Scale to the largest unit below `base` after rounding to the displayed
/// precision, so 1023.96 KiB becomes "1.0 MiB" rather than "1024.0 KiB".
fn scaled(
    value: f64,
    base: f64,
    units: &'static [&'static str],
    decimals: impl Fn(usize) -> usize,
) -> (f64, &'static str, usize) {
    let mut value = if value.is_finite() { value } else { 0.0 };
    let mut index = 0;
    loop {
        let factor = 10f64.powi(decimals(index) as i32);
        let rounded = (value * factor).round() / factor;
        if rounded < base || index + 1 == units.len() {
            return (rounded, units[index], decimals(index));
        }
        value /= base;
        index += 1;
    }
}

/// Truncate to at most `width` characters, marking the cut with "~".
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('~');
    out
}

/// Keep the end of a path when it does not fit, as shells do in prompts.
pub fn truncate_left(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::from("~");
    out.extend(text.chars().skip(count - (width - 1)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_escapes_controls_bidi_and_invalid_bytes_distinctly() {
        assert_eq!(sanitize("plain name"), "plain name");
        assert_eq!(sanitize("a\nb\tc\x1b[31m"), "a\\x0ab\\x09c\\x1b[31m");
        assert_eq!(sanitize("x\u{202e}y"), "x\\u{202e}y");
        assert_eq!(sanitize("\u{85}"), "\\u{0085}");
        assert_eq!(sanitize("Grüße 日本"), "Grüße 日本");
        assert_eq!(sanitize_bytes(b"caf\xe9"), "caf\\xe9");
        assert_eq!(sanitize_bytes(b"\xff\xfe"), "\\xff\\xfe");
        assert_eq!(sanitize_bytes(b"ok\xc3"), "ok\\xc3");
        // A literal "\xe9" name must not look like the invalid byte 0xe9.
        assert_ne!(sanitize_bytes(b"caf\\xe9"), sanitize_bytes(b"caf\xe9"));
        assert!(
            sanitize_bytes(&(0u8..=255).collect::<Vec<_>>())
                .chars()
                .all(|c| !c.is_control())
        );
    }

    #[test]
    fn units_round_into_the_next_unit_and_stay_narrow() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(1023.4), "1023 B");
        assert_eq!(format_bytes(1023.6), "1.0 KiB");
        assert_eq!(format_bytes(1023.96 * 1024.0), "1.0 MiB");
        assert_eq!(format_bytes(1023.94 * 1024.0), "1023.9 KiB");
        assert_eq!(format_bytes(f64::NAN), "0 B");
        assert_eq!(format_bytes(-5.0), "0 B");
        assert_eq!(format_rate(1536.0), "1.5 KiB/s");
        let mut value = 0.5;
        while value < 1e17 {
            assert!(format_bytes(value).len() <= 10, "{}", format_bytes(value));
            value *= 1.0007;
        }
        assert_eq!(format_delta(0), "0 B");
        assert_eq!(format_delta(-2048), "-2.0 KiB");
        assert_eq!(format_delta(i64::MIN), "-8192.0 PiB");
        assert_eq!(format_delta(1536), "+1.5 KiB");
        assert_eq!(format_bytes_short(512.0), "512B");
        assert_eq!(format_bytes_short(1023.6), "1.0K");
        assert_eq!(format_bytes_short(12.34 * 1024.0 * 1024.0), "12.3M");
        assert_eq!(format_bytes_short(160.4 * 1024.0 * 1024.0), "160M");
        assert_eq!(format_bytes_short(1.4 * 1024f64.powi(4)), "1.4T");
        assert_eq!(format_bytes_short(f64::NAN), "0B");
        let mut value = 0.5;
        while value < 1e17 {
            assert!(
                format_bytes_short(value).len() <= 5,
                "{}",
                format_bytes_short(value)
            );
            value *= 1.0007;
        }
        assert_eq!(format_bytes_tiny(5.94 * 1024.0 * 1024.0), "5.9M");
        assert_eq!(format_bytes_tiny(79.7 * 1024.0 * 1024.0), "80M");
        assert_eq!(format_bytes_tiny(1010.0), "1.0K");
        let mut value = 0.5;
        while value < 1e17 {
            assert!(
                format_bytes_tiny(value).len() <= 4,
                "{}",
                format_bytes_tiny(value)
            );
            value *= 1.0007;
        }
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_short_count(999.4), "999");
        assert_eq!(format_short_count(999.6), "1.0k");
        assert_eq!(format_short_count(f64::INFINITY), "0");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(3600 + 120), "1h 2m");
        assert_eq!(format_duration(7200), "2h");
        assert_eq!(format_duration(200_000), "2d");
        assert_eq!(truncate("abcdef", 4), "abc~");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate_left("/home/tim/projects", 8), "~rojects");
    }
}
