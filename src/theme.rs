//! Theme colours, the built-in logo mark and the per-request CSS variables.

use anyhow::{bail, Result};

/// Default accent: a deep estuary teal.
pub const DEFAULT_ACCENT: &str = "#0f6f73";

/// The dunlin mark: a plump shorebird with a long, slightly down-curved bill.
/// Single colour through `currentColor` so it follows the accent; the eye is a
/// hole (even-odd fill) rather than a second colour for the same reason.
pub const LOGO_SVG: &str = r#"<svg class="mark" viewBox="0 0 32 32" aria-hidden="true" focusable="false"><path fill="currentColor" fill-rule="evenodd" d="M7.8 12.3C8 10 9.5 8.8 11.5 8.8c2.5 0 4 1.7 5.5 3.2 5 0 10.5 1.5 14 4.5-2.5 1-4.5 2-6 3-2.5 4.5-9 6-12.5 4-2.5-1.5-3.9-4.5-3.7-7.5 0-.8-.4-1.4-.8-1.7-3.8.7-6.6 5.3-6.6 5.3.6-3.4 2.2-6.9 6.2-7.3zM10.1 11.6a.9.9 0 1 0 1.8 0 .9.9 0 1 0-1.8 0z"/><path d="M15 24.3l-1 5.2M18.2 24.3l.4 5.2" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" fill="none"/></svg>"#;

/// Favicon with the accent baked in; browsers do not apply page CSS to icons.
pub fn favicon_svg(accent: &str) -> String {
    LOGO_SVG
        .replace(r#" class="mark""#, r#" xmlns="http://www.w3.org/2000/svg""#)
        .replace(r#" aria-hidden="true" focusable="false""#, "")
        .replace("currentColor", accent)
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

const WHITE: Rgb = Rgb(255, 255, 255);
const BLACK: Rgb = Rgb(0, 0, 0);
/// Surfaces the accent is drawn on; must match `--surface` in style.css.
const LIGHT_SURFACE: Rgb = Rgb(255, 255, 255);
const DARK_SURFACE: Rgb = Rgb(0x16, 0x1c, 0x23);

/// Step the colour towards `target` until it reaches 4.5:1 against `bg`, so any
/// configured accent still gives readable links in both themes.
fn ensure_contrast(c: Rgb, bg: Rgb, target: Rgb) -> Rgb {
    let mut t = 0.0;
    let mut out = c;
    while contrast(out, bg) < 4.5 && t < 1.0 {
        t += 0.05;
        out = c.mix(target, t);
    }
    out
}

/// Text colour for solid accent backgrounds (buttons).
fn ink_for(bg: Rgb) -> Rgb {
    if contrast(WHITE, bg) >= contrast(Rgb(0x0b, 0x12, 0x18), bg) {
        WHITE
    } else {
        Rgb(0x0b, 0x12, 0x18)
    }
}

/// CSS custom properties for the accent in light and dark mode.
pub fn accent_css(accent: &str) -> String {
    let base = parse_hex(accent).unwrap_or(Rgb(0x0f, 0x6f, 0x73));
    let light = ensure_contrast(base, LIGHT_SURFACE, BLACK);
    let dark = ensure_contrast(base, DARK_SURFACE, WHITE);
    let vars = |c: Rgb| format!("--accent:{};--accent-ink:{};", c.hex(), ink_for(c).hex());
    format!(
        ":root{{{l}}}:root[data-theme=dark]{{{d}}}@media (prefers-color-scheme:dark){{:root[data-theme=auto]{{{d}}}}}",
        l = vars(light),
        d = vars(dark),
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
    fn accent_is_readable_in_both_modes() {
        // A very light accent must be darkened for light mode and kept for dark.
        let css = accent_css("#f0f0a0");
        assert!(css.contains("data-theme=dark"));
        for accent in ["#0f6f73", "#f0f0a0", "#101010", "#ff0000"] {
            let base = parse_hex(accent).unwrap();
            assert!(contrast(ensure_contrast(base, LIGHT_SURFACE, BLACK), LIGHT_SURFACE) >= 4.5);
            assert!(contrast(ensure_contrast(base, DARK_SURFACE, WHITE), DARK_SURFACE) >= 4.5);
        }
    }

    #[test]
    fn favicon_uses_accent() {
        let svg = favicon_svg("#123456");
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains("#123456"));
        assert!(!svg.contains("currentColor"));
    }
}
