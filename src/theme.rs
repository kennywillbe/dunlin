//! Theme colours, the built-in logo mark and the per-request CSS variables.

use anyhow::{bail, Result};

/// Default accent: the ink colour, so an unconfigured page has no colour
/// except where something is wrong.
pub const DEFAULT_ACCENT: &str = "#17150f";

/// Ink colour of the paper theme; must match `--ink` in style.css.
pub const INK: &str = "#17150f";

/// The dunlin mark: a round shorebird with a long bill, on two thin legs.
/// Single colour through `currentColor`; the eye is a hole (even-odd fill)
/// rather than a second colour so the mark works on any background.
pub const LOGO_SVG: &str = r#"<svg class="mark" viewBox="0 0 32 32" aria-hidden="true" focusable="false"><path fill="currentColor" fill-rule="evenodd" d="M4 18.5c0-5 4-8.2 9.2-8.2 3 0 5.2 1.1 6.3 3.1l9.5 3.6-9.4-.6c-1.2 3-4.2 4.9-8.3 4.9-4.3 0-7.3-1-7.3-2.8zM16.1 13.8a1.1 1.1 0 1 0 2.2 0 1.1 1.1 0 1 0-2.2 0z"/><path d="M11.5 23.3l-.8 5.2M15.5 23.2l.6 5.3" stroke="currentColor" stroke-width="1.6" fill="none"/></svg>"#;

/// Favicon in ink; browsers do not apply page CSS to icons.
pub fn favicon_svg() -> String {
    LOGO_SVG
        .replace(r#" class="mark""#, r#" xmlns="http://www.w3.org/2000/svg""#)
        .replace(r#" aria-hidden="true" focusable="false""#, "")
        .replace("currentColor", INK)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }
}

/// WCAG contrast ratio.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Parse strictly `#rrggbb`; shorthand and names are rejected so the value can
/// be emitted into CSS without any escaping concerns.
pub fn parse_hex(s: &str) -> Result<Rgb> {
    let b = s.as_bytes();
    if b.len() != 7 || b[0] != b'#' || !b[1..].iter().all(u8::is_ascii_hexdigit) {
        bail!("accent {s:?} must be a colour like \"#0f6f73\"");
    }
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0);
    Ok(Rgb(p(1), p(3), p(5)))
}

const PAPER: Rgb = Rgb(0xf1, 0xf0, 0xec);
const INK_RGB: Rgb = Rgb(0x17, 0x15, 0x0f);

/// Step the colour towards ink until it reaches 4.5:1 against the paper, so
/// any configured accent still gives readable links.
fn ensure_contrast(c: Rgb) -> Rgb {
    let mut t = 0.0;
    let mut out = c;
    while contrast(out, PAPER) < 4.5 && t < 1.0 {
        t += 0.05;
        out = c.mix(INK_RGB, t);
    }
    out
}

/// Text colour for solid accent backgrounds (buttons).
fn ink_for(bg: Rgb) -> Rgb {
    if contrast(PAPER, bg) >= contrast(INK_RGB, bg) {
        PAPER
    } else {
        INK_RGB
    }
}

/// CSS custom properties for the accent.
pub fn accent_css(accent: &str) -> String {
    let base = parse_hex(accent).unwrap_or(INK_RGB);
    let c = ensure_contrast(base);
    format!(
        ":root{{--accent:{};--accent-ink:{};}}",
        c.hex(),
        ink_for(c).hex()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing_is_strict() {
        assert_eq!(parse_hex("#0f6f73").unwrap(), Rgb(0x0f, 0x6f, 0x73));
        assert_eq!(parse_hex("#ABCDEF").unwrap(), Rgb(0xab, 0xcd, 0xef));
        for bad in ["0f6f73", "#fff", "#0f6f7", "#0f6f733", "#0g6f73", "red", ""] {
            assert!(parse_hex(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn accent_is_readable_on_paper() {
        assert_eq!(
            accent_css(DEFAULT_ACCENT),
            ":root{--accent:#17150f;--accent-ink:#f1f0ec;}"
        );
        for accent in ["#0f6f73", "#f0f0a0", "#101010", "#ff0000"] {
            let c = ensure_contrast(parse_hex(accent).unwrap());
            assert!(contrast(c, PAPER) >= 4.5, "{accent}");
        }
        // A light accent is darkened rather than used as is.
        assert!(!accent_css("#f0f0a0").contains("#f0f0a0"));
    }

    #[test]
    fn favicon_is_ink() {
        let svg = favicon_svg();
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains(INK));
        assert!(!svg.contains("currentColor"));
    }
}
