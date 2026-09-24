//! Prometheus text exposition format 0.0.4, written by hand: the format is a
//! few lines of text and a client crate would bring its own registry.

use std::fmt::Write;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// One gauge family: its HELP/TYPE header and every series under it. Every
/// metric dunlin exports is a gauge.
pub struct Family {
    name: &'static str,
    help: &'static str,
    samples: Vec<(Vec<(&'static str, String)>, f64)>,
}

impl Family {
    pub fn gauge(name: &'static str, help: &'static str) -> Self {
        Self {
            name,
            help,
            samples: Vec::new(),
        }
    }

    pub fn push(&mut self, labels: Vec<(&'static str, String)>, value: f64) {
        self.samples.push((labels, value));
    }
}

fn escape_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// HELP text escapes only backslash and newline; quotes stay as they are.
fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

fn format_value(v: f64) -> String {
    // Rust prints "inf" and "NaN"; the format wants "+Inf", "-Inf", "NaN".
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else {
        v.to_string()
    }
}

/// Render families in order. A family with no series still gets its header,
/// so the metric list is visible before anything has been measured.
pub fn render(families: &[Family]) -> String {
    let mut out = String::new();
    for f in families {
        let _ = writeln!(out, "# HELP {} {}", f.name, escape_help(f.help));
        let _ = writeln!(out, "# TYPE {} gauge", f.name);
        for (labels, value) in &f.samples {
            out.push_str(f.name);
            if !labels.is_empty() {
                let parts: Vec<String> = labels
                    .iter()
                    .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
                    .collect();
                let _ = write!(out, "{{{}}}", parts.join(","));
            }
            let _ = writeln!(out, " {}", format_value(*value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headers_labels_and_values() {
        let mut up = Family::gauge("dunlin_check_up", "Whether the check passed.");
        up.push(vec![("check", "web".into())], 1.0);
        up.push(vec![("check", "a\\b \"c\"\nd".into())], 0.0);
        let mut info = Family::gauge("dunlin_build_info", "Build.\nSecond \\ line");
        info.push(vec![], 1.0);
        let empty = Family::gauge("dunlin_empty", "Nothing yet.");
        let mut odd = Family::gauge("dunlin_odd", "Odd values.");
        odd.push(vec![("a", "x".into()), ("b", "y".into())], 0.25);
        odd.push(vec![], f64::INFINITY);
        odd.push(vec![], f64::NEG_INFINITY);
        odd.push(vec![], f64::NAN);

        assert_eq!(
            render(&[up, info, empty, odd]),
            "# HELP dunlin_check_up Whether the check passed.\n\
             # TYPE dunlin_check_up gauge\n\
             dunlin_check_up{check=\"web\"} 1\n\
             dunlin_check_up{check=\"a\\\\b \\\"c\\\"\\nd\"} 0\n\
             # HELP dunlin_build_info Build.\\nSecond \\\\ line\n\
             # TYPE dunlin_build_info gauge\n\
             dunlin_build_info 1\n\
             # HELP dunlin_empty Nothing yet.\n\
             # TYPE dunlin_empty gauge\n\
             # HELP dunlin_odd Odd values.\n\
             # TYPE dunlin_odd gauge\n\
             dunlin_odd{a=\"x\",b=\"y\"} 0.25\n\
             dunlin_odd +Inf\n\
             dunlin_odd -Inf\n\
             dunlin_odd NaN\n"
        );
    }
}
