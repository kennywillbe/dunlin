//! Shields-style status badges, drawn here so no renderer or font is needed.

use crate::models::State;

/// Grey for a badge that has nothing to report (no probe data, unknown id).
pub const NO_DATA: u32 = 0x9f9f9f;

const LABEL_BG: u32 = 0x555555;

/// Advance widths of printable ASCII (0x20..=0x7e) in Verdana at 11px, in
/// tenths of a pixel. Shields draws with Verdana, and badges only have to
/// look right, so estimated widths are enough.
#[rustfmt::skip]
const VERDANA_11: [u8; 95] = [
    // space ! " # $ % & ' ( ) * + , - . /
    39, 44, 50, 92, 70, 119, 86, 30, 57, 57, 70, 92, 40, 46, 40, 57,
    // 0-9
    70, 70, 70, 70, 70, 70, 70, 70, 70, 70,
    // : ; < = > ? @
    48, 48, 92, 92, 92, 60, 110,
    // A-Z
    75, 76, 77, 85, 69, 63, 86, 83, 46, 47, 76, 62, 94, 82, 88, 69, 88, 78, 75, 67, 81, 75, 109, 76, 67, 76,
    // [ \ ] ^ _ `
    57, 57, 57, 92, 70, 70,
    // a-z
    66, 69, 57, 69, 66, 39, 69, 70, 30, 38, 65, 30, 107, 70, 67, 69, 69, 47, 57, 43, 70, 65, 90, 65, 65, 56,
    // { | } ~
    70, 57, 70, 92,
];

/// Estimated rendered width of `text` in pixels at 11px Verdana.
pub fn text_width(text: &str) -> f64 {
    let tenths: u32 = text
        .chars()
        .map(|c| match c {
            ' '..='~' => u32::from(VERDANA_11[c as usize - 0x20]),
            // CJK and emoji run about a full em; other scripts are close
            // to an average Latin letter.
            c if c >= '\u{2e80}' => 110,
            _ => 70,
        })
        .sum();
    f64::from(tenths) / 10.0
}

/// Status wording on the badge, e.g. "partial outage".
pub fn state_message(state: State) -> String {
    state.as_str().replace('_', " ")
}

/// Colour bands for the 90-day uptime figure.
pub fn uptime_color(pct: Option<f64>) -> u32 {
    match pct {
        None => NO_DATA,
        Some(p) if p >= 99.9 => State::Operational.rgb(),
        Some(p) if p >= 99.0 => State::Degraded.rgb(),
        Some(p) if p >= 95.0 => State::PartialOutage.rgb(),
        Some(_) => State::MajorOutage.rgb(),
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub struct Badge {
    pub label: String,
    pub message: String,
    pub color: u32,
}

impl Badge {
    /// Flat two-part badge, the same layout shields.io and Healthchecks use.
    pub fn svg(&self) -> String {
        const PAD: f64 = 5.0;
        let lt = text_width(&self.label).ceil();
        let mt = text_width(&self.message).ceil();
        let lw = lt + 2.0 * PAD;
        let mw = mt + 2.0 * PAD;
        let w = lw + mw;
        let (lx, mx) = (lw / 2.0, lw + mw / 2.0);
        let label = escape(&self.label);
        let message = escape(&self.message);
        let full = format!("{label}: {message}");
        let (lbg, color) = (LABEL_BG, self.color);
        // textLength pins each text to the estimated width, so a wrong
        // estimate squeezes the text a little instead of overflowing.
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" aria-label="{full}"><title>{full}</title><linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient><clipPath id="r"><rect width="{w}" height="20" rx="3" fill="#fff"/></clipPath><g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#{lbg:06x}"/><rect x="{lw}" width="{mw}" height="20" fill="#{color:06x}"/><rect width="{w}" height="20" fill="url(#s)"/></g><g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" text-rendering="geometricPrecision" font-size="11"><text aria-hidden="true" x="{lx}" y="15" fill="#010101" fill-opacity=".3" textLength="{lt}">{label}</text><text x="{lx}" y="14" textLength="{lt}">{label}</text><text aria-hidden="true" x="{mx}" y="15" fill="#010101" fill-opacity=".3" textLength="{mt}">{message}</text><text x="{mx}" y="14" textLength="{mt}">{message}</text></g></svg>"##
        )
    }

    /// shields.io endpoint schema, for restyling the badge through shields.
    pub fn endpoint_json(&self) -> String {
        serde_json::json!({
            "schemaVersion": 1,
            "label": self.label,
            "message": self.message,
            "color": format!("{:06x}", self.color),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_follow_the_table() {
        assert_eq!(text_width(""), 0.0);
        assert_eq!(text_width("0"), 7.0);
        assert!((text_width("up") - 13.9).abs() < 1e-9);
        // Narrow letters really are narrower than wide ones.
        assert!(text_width("iiii") < text_width("WWWW"));
        assert_eq!(text_width("ş"), 7.0);
        assert_eq!(text_width("監"), 11.0);
        assert_eq!(VERDANA_11[usize::from(b'~' - 0x20)], 92);
    }

    #[test]
    fn uptime_bands() {
        assert_eq!(uptime_color(None), NO_DATA);
        assert_eq!(uptime_color(Some(100.0)), State::Operational.rgb());
        assert_eq!(uptime_color(Some(99.9)), State::Operational.rgb());
        assert_eq!(uptime_color(Some(99.89)), State::Degraded.rgb());
        assert_eq!(uptime_color(Some(99.0)), State::Degraded.rgb());
        assert_eq!(uptime_color(Some(98.9)), State::PartialOutage.rgb());
        assert_eq!(uptime_color(Some(95.0)), State::PartialOutage.rgb());
        assert_eq!(uptime_color(Some(94.9)), State::MajorOutage.rgb());
    }

    #[test]
    fn state_wording() {
        assert_eq!(state_message(State::PartialOutage), "partial outage");
        assert_eq!(state_message(State::Maintenance), "maintenance");
    }

    #[test]
    fn svg_escapes_and_sizes() {
        let b = Badge {
            label: r#"A<&">'"#.into(),
            message: "operational".into(),
            color: State::Operational.rgb(),
        };
        let svg = b.svg();
        assert!(!svg.contains(r#"A<&">'"#));
        assert!(
            svg.contains("A&lt;&amp;&quot;&gt;&apos;: operational"),
            "{svg}"
        );
        assert!(svg.contains(r#"role="img""#));
        assert!(svg.contains("fill=\"#2e9e5b\""));
        let lw = text_width(r#"A<&">'"#).ceil() + 10.0;
        assert!(
            svg.contains(&format!(r#"<rect width="{lw}" height="20""#)),
            "{svg}"
        );
    }

    #[test]
    fn endpoint_json_shape() {
        let b = Badge {
            label: "Website".into(),
            message: "99.95%".into(),
            color: 0x2e9e5b,
        };
        let v: serde_json::Value = serde_json::from_str(&b.endpoint_json()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "schemaVersion": 1,
                "label": "Website",
                "message": "99.95%",
                "color": "2e9e5b",
            })
        );
    }
}
