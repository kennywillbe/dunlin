//! The sentence at the top of every page: what is wrong right now, said the
//! way a person would say it, built from the same state the data column shows.

use crate::models::State;
use crate::templates::TimeView;

/// A piece of a sentence. Kept structured instead of pre-rendered HTML so the
/// template escapes every name and message on its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    /// A component name, coloured by the class (`bad`, `warn`, `mnt`).
    Name(String, &'static str),
    Strong(String),
    Time(TimeView),
    Link(String, String),
}

fn text(s: impl Into<String>) -> Part {
    Part::Text(s.into())
}

/// The headline and the paragraph under it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Say {
    pub headline: Vec<Part>,
    pub detail: Vec<Part>,
}

/// What the sentence needs to know about one component.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub name: String,
    pub state: State,
    /// Start of the open incident on this component, if any.
    pub since: Option<i64>,
    pub latency_ms: Option<f64>,
    /// End of the maintenance window it is in, if any.
    pub maintenance_until: Option<i64>,
    /// A disk, memory, swap or load check: degraded means a number is high,
    /// not that something answers slowly.
    pub resource: bool,
}

/// The open incident whose latest update leads the detail paragraph.
#[derive(Debug, Clone)]
pub struct OpenIncident {
    pub id: i64,
    pub message: Option<String>,
}

/// Maintenance that has not started yet.
#[derive(Debug, Clone)]
pub struct Upcoming {
    pub component: String,
    pub starts: i64,
}

/// Everything the status sentence is built from.
#[derive(Debug, Clone, Default)]
pub struct StatusFacts<'a> {
    pub components: &'a [Snapshot],
    pub incident: Option<OpenIncident>,
    pub last_resolved: Option<i64>,
    pub upcoming: &'a [Upcoming],
    pub now: i64,
}

/// `11 minutes`, `2 hours`, `3 days`: one unit, rounded down, because the
/// headline should read like speech, not like a stopwatch.
pub fn humanize(secs: i64) -> String {
    let secs = secs.max(0);
    let (n, unit) = if secs < 60 {
        return "less than a minute".to_string();
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else {
        (secs / 86_400, "day")
    };
    plural(n, unit)
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Small counts read better as words in a sentence ("the other five").
pub fn number_word(n: usize) -> String {
    const WORDS: [&str; 13] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve",
    ];
    WORDS
        .get(n)
        .map_or_else(|| n.to_string(), |w| (*w).to_string())
}

/// `84 ms` below a second, `1.4 s` above it.
pub fn fmt_latency(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

/// `a`, `a and b`, `a, b and c`, `a, b and 2 more`.
fn name_list(names: &[&str], class: &'static str) -> Vec<Part> {
    let name = |s: &str| Part::Name(s.to_string(), class);
    match names {
        [] => Vec::new(),
        [a] => vec![name(a)],
        [a, b] => vec![name(a), text(" and "), name(b)],
        [a, b, c] => vec![name(a), text(", "), name(b), text(" and "), name(c)],
        [a, b, rest @ ..] => vec![
            name(a),
            text(", "),
            name(b),
            text(format!(" and {} more", rest.len())),
        ],
    }
}

fn class_for(state: State) -> &'static str {
    match state {
        State::MajorOutage | State::PartialOutage => "bad",
        State::Degraded => "warn",
        State::Maintenance => "mnt",
        State::Operational => "",
    }
}

/// Ends a free-text message with a full stop unless it already has one.
fn sentence_end(msg: &str) -> String {
    let msg = msg.trim();
    if msg.ends_with(['.', '!', '?']) {
        msg.to_string()
    } else {
        format!("{msg}.")
    }
}

fn headline_for(state: State, group: &[&Snapshot], all: usize, now: i64) -> Vec<Part> {
    let names: Vec<&str> = group.iter().map(|c| c.name.as_str()).collect();
    let many = group.len() > 1;
    let mut out = if state == State::Maintenance && many && group.len() == all {
        vec![text("Everything")]
    } else {
        name_list(&names, class_for(state))
    };
    match state {
        State::MajorOutage if !many => match group[0].since {
            Some(since) => out.push(text(format!(
                " has been down for {}.",
                humanize(now - since)
            ))),
            None => out.push(text(" is down.")),
        },
        State::MajorOutage => out.push(text(" are down.")),
        State::PartialOutage => out.push(text(if many {
            " are having trouble."
        } else {
            " is having trouble."
        })),
        State::Degraded if group.iter().all(|c| c.resource) => out.push(text(if many {
            " are running high."
        } else {
            " is running high."
        })),
        State::Degraded => out.push(text(if many { " are slow." } else { " is slow." })),
        State::Maintenance => {
            let verb = if many && group.len() != all {
                " are"
            } else {
                " is"
            };
            match group.iter().filter_map(|c| c.maintenance_until).max() {
                Some(until) => {
                    out.push(text(format!("{verb} under maintenance until ")));
                    out.push(Part::Time(TimeView::time(until)));
                    out.push(text("."));
                }
                None => out.push(text(format!("{verb} under maintenance."))),
            }
        }
        State::Operational => {}
    }
    out
}

/// Status page sentence.
pub fn status_say(f: &StatusFacts<'_>) -> Say {
    let comps = f.components;
    if comps.is_empty() {
        return Say {
            headline: vec![text("Nothing to watch yet.")],
            detail: vec![text(
                "Add components to the config file and they show up here.",
            )],
        };
    }
    let worst = comps
        .iter()
        .map(|c| c.state)
        .max()
        .unwrap_or(State::Operational);
    let mut detail = Vec::new();
    if let Some(inc) = &f.incident {
        if let Some(m) = inc.message.as_deref().filter(|m| !m.trim().is_empty()) {
            detail.push(text(format!("{} ", sentence_end(m))));
        }
        detail.push(Part::Link(
            "Follow the incident.".to_string(),
            format!("/incidents/{}", inc.id),
        ));
    }

    let headline = if worst == State::Operational {
        push_sentence(&mut detail, vec![text(quiet_since(f.last_resolved, f.now))]);
        vec![text("Everything is up.")]
    } else {
        let group: Vec<&Snapshot> = comps.iter().filter(|c| c.state == worst).collect();
        let headline = headline_for(worst, &group, comps.len(), f.now);
        for c in comps
            .iter()
            .filter(|c| c.state != worst && c.state != State::Operational)
        {
            push_sentence(&mut detail, aside(c));
        }
        if worst == State::Degraded {
            for c in &group {
                if let Some(ms) = c.latency_ms {
                    push_sentence(
                        &mut detail,
                        vec![
                            text(format!("{} answers in ", c.name)),
                            Part::Strong(fmt_latency(ms)),
                            text("."),
                        ],
                    );
                }
            }
        }
        let fine = comps
            .iter()
            .filter(|c| c.state == State::Operational)
            .count();
        match fine {
            0 => {}
            1 => push_sentence(&mut detail, vec![text("The other one is fine.")]),
            n => push_sentence(
                &mut detail,
                vec![text(format!("The other {} are fine.", number_word(n)))],
            ),
        }
        headline
    };

    for u in f.upcoming {
        push_sentence(
            &mut detail,
            vec![
                text(format!("Maintenance on {} starts ", u.component)),
                Part::Time(TimeView::datetime(u.starts)),
                text("."),
            ],
        );
    }
    Say { headline, detail }
}

/// One sentence about a component that is not the headline's subject.
fn aside(c: &Snapshot) -> Vec<Part> {
    let name = c.name.clone();
    match c.state {
        State::Degraded if c.resource => vec![text(format!("{name} is running high."))],
        State::Degraded => match c.latency_ms {
            Some(ms) => vec![
                text(format!("{name} is answering, but slowly (")),
                Part::Strong(fmt_latency(ms)),
                text(")."),
            ],
            None => vec![text(format!("{name} is slow."))],
        },
        State::PartialOutage => vec![text(format!("{name} is having trouble."))],
        State::MajorOutage => vec![text(format!("{name} is down."))],
        State::Maintenance => vec![text(format!("{name} is under maintenance."))],
        State::Operational => Vec::new(),
    }
}

fn push_sentence(detail: &mut Vec<Part>, parts: Vec<Part>) {
    if parts.is_empty() {
        return;
    }
    if let Some(Part::Text(last)) = detail.last() {
        if !last.ends_with(' ') {
            detail.push(text(" "));
        }
    } else if !detail.is_empty() {
        detail.push(text(" "));
    }
    detail.extend(parts);
}

fn quiet_since(last_resolved: Option<i64>, now: i64) -> String {
    match last_resolved {
        None => "Nothing has broken yet.".to_string(),
        Some(t) => match (now - t).max(0) / 86_400 {
            0 => "Nothing has broken since earlier today.".to_string(),
            n => format!("Nothing has broken in {}.", plural(n, "day")),
        },
    }
}

/// One host number for the metrics sentence.
#[derive(Debug, Clone)]
pub struct HostMetric {
    pub kind: HostKind,
    /// Percent, 0 to 100.
    pub value: f64,
    /// Percent at which it counts as a problem.
    pub warn: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostKind {
    Cpu,
    Memory,
    Disk(String),
}

impl HostMetric {
    fn phrase(&self, only_disk: bool) -> String {
        let v = self.value.round();
        match &self.kind {
            HostKind::Cpu => format!("{v}% CPU"),
            HostKind::Memory => format!("{v}% memory"),
            HostKind::Disk(_) if only_disk => format!("disk {v}% full"),
            HostKind::Disk(m) => format!("disk {m} {v}% full"),
        }
    }
}

/// Metrics page sentence: the host in one line, the worst number first when
/// something is over its line.
pub fn host_say(metrics: &[HostMetric]) -> Say {
    if metrics.is_empty() {
        return Say {
            headline: vec![text("No numbers from the server yet.")],
            detail: vec![text(
                "The collector takes its first sample a minute after start.",
            )],
        };
    }
    let only_disk = metrics
        .iter()
        .filter(|m| matches!(m.kind, HostKind::Disk(_)))
        .count()
        == 1;
    let list = |ms: &[&HostMetric]| {
        ms.iter()
            .map(|m| m.phrase(only_disk))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let worst = metrics
        .iter()
        .filter(|m| m.value >= m.warn)
        .max_by(|a, b| (a.value / a.warn).total_cmp(&(b.value / b.warn)));
    let Some(worst) = worst else {
        let all: Vec<&HostMetric> = metrics.iter().collect();
        return Say {
            headline: vec![text(format!("The server is fine: {}.", list(&all)))],
            detail: Vec::new(),
        };
    };
    let v = worst.value.round();
    let headline = match &worst.kind {
        HostKind::Cpu => vec![
            text("The "),
            Part::Name("CPU".to_string(), "bad"),
            text(format!(" is busy: {v}%.")),
        ],
        HostKind::Memory => vec![
            Part::Name("Memory".to_string(), "bad"),
            text(format!(" is almost full: {v}% used.")),
        ],
        HostKind::Disk(m) => vec![
            Part::Name(format!("Disk {m}"), "bad"),
            text(format!(" is almost full: {v}% used.")),
        ],
    };
    let mut detail = vec![text(format!("Trouble starts at {}%.", worst.warn.round()))];
    let rest: Vec<&HostMetric> = metrics.iter().filter(|m| m.kind != worst.kind).collect();
    if !rest.is_empty() {
        detail.push(text(format!(" The rest: {}.", list(&rest))));
    }
    Say { headline, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_758_700_000;

    fn comp(name: &str, state: State) -> Snapshot {
        Snapshot {
            name: name.to_string(),
            state,
            since: None,
            latency_ms: None,
            maintenance_until: None,
            resource: false,
        }
    }

    /// Flatten parts to plain text; names are wrapped in `*` so tests can see
    /// which words get the colour.
    fn flat(parts: &[Part]) -> String {
        parts
            .iter()
            .map(|p| match p {
                Part::Text(s) | Part::Strong(s) => s.clone(),
                Part::Name(s, _) => format!("*{s}*"),
                Part::Time(t) => t.text.clone(),
                Part::Link(s, _) => s.clone(),
            })
            .collect()
    }

    fn say(comps: &[Snapshot]) -> Say {
        status_say(&StatusFacts {
            components: comps,
            now: NOW,
            ..Default::default()
        })
    }

    #[test]
    fn durations_read_like_speech() {
        assert_eq!(humanize(20), "less than a minute");
        assert_eq!(humanize(60), "1 minute");
        assert_eq!(humanize(11 * 60 + 40), "11 minutes");
        assert_eq!(humanize(3600), "1 hour");
        assert_eq!(humanize(2 * 3600 + 59 * 60), "2 hours");
        assert_eq!(humanize(86_400), "1 day");
        assert_eq!(humanize(3 * 86_400 + 5000), "3 days");
        assert_eq!(humanize(-5), "less than a minute");
    }

    #[test]
    fn one_down_says_for_how_long() {
        let mut down = comp("dawnwire", State::MajorOutage);
        down.since = Some(NOW - 11 * 60);
        let s = say(&[down, comp("ksnip", State::Operational)]);
        assert_eq!(
            flat(&s.headline),
            "*dawnwire* has been down for 11 minutes."
        );
        assert!(matches!(&s.headline[0], Part::Name(_, "bad")));
        assert_eq!(flat(&s.detail), "The other one is fine.");

        let s = say(&[comp("dawnwire", State::MajorOutage)]);
        assert_eq!(flat(&s.headline), "*dawnwire* is down.");
    }

    #[test]
    fn several_down_are_listed() {
        let two = [comp("a", State::MajorOutage), comp("b", State::MajorOutage)];
        assert_eq!(flat(&say(&two).headline), "*a* and *b* are down.");
        let three = [
            comp("a", State::MajorOutage),
            comp("b", State::MajorOutage),
            comp("c", State::MajorOutage),
        ];
        assert_eq!(flat(&say(&three).headline), "*a*, *b* and *c* are down.");
        let four: Vec<Snapshot> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| comp(n, State::MajorOutage))
            .collect();
        assert_eq!(flat(&say(&four).headline), "*a*, *b* and 2 more are down.");
    }

    #[test]
    fn slow_and_trouble_and_maintenance() {
        let mut slow = comp("btccaster", State::Degraded);
        slow.latency_ms = Some(1400.0);
        let s = say(&[slow.clone(), comp("ksnip", State::Operational)]);
        assert_eq!(flat(&s.headline), "*btccaster* is slow.");
        assert!(matches!(&s.headline[0], Part::Name(_, "warn")));
        assert_eq!(
            flat(&s.detail),
            "btccaster answers in 1.4 s. The other one is fine."
        );
        let s = say(&[slow.clone(), comp("x", State::Degraded)]);
        assert_eq!(flat(&s.headline), "*btccaster* and *x* are slow.");

        let s = say(&[comp("api", State::PartialOutage)]);
        assert_eq!(flat(&s.headline), "*api* is having trouble.");

        let mut m = comp("Root disk", State::Maintenance);
        m.maintenance_until = Some(NOW - NOW % 86_400 + 14 * 3600 + 30 * 60);
        let s = say(&[m.clone(), comp("ksnip", State::Operational)]);
        assert_eq!(
            flat(&s.headline),
            "*Root disk* is under maintenance until 14:30 UTC."
        );
        let s = say(&[m.clone(), m.clone()]);
        assert_eq!(
            flat(&s.headline),
            "Everything is under maintenance until 14:30 UTC."
        );
    }

    #[test]
    fn detail_leads_with_the_incident_then_slow_ones_then_the_rest() {
        let mut down = comp("dawnwire", State::MajorOutage);
        down.since = Some(NOW - 2 * 3600 - 5);
        let mut slow = comp("btccaster", State::Degraded);
        slow.latency_ms = Some(1400.0);
        let comps = [
            down,
            slow,
            comp("a", State::Operational),
            comp("b", State::Operational),
            comp("c", State::Operational),
            comp("d", State::Operational),
            comp("e", State::Operational),
        ];
        let s = status_say(&StatusFacts {
            components: &comps,
            incident: Some(OpenIncident {
                id: 7,
                message: Some("Its HTTPS check failed three times in a row".to_string()),
            }),
            now: NOW,
            ..Default::default()
        });
        assert_eq!(flat(&s.headline), "*dawnwire* has been down for 2 hours.");
        assert_eq!(
            flat(&s.detail),
            "Its HTTPS check failed three times in a row. Follow the incident. \
             btccaster is answering, but slowly (1.4 s). The other five are fine."
        );
        assert!(s.detail.contains(&Part::Link(
            "Follow the incident.".into(),
            "/incidents/7".into()
        )));
    }

    #[test]
    fn all_fine_counts_quiet_days() {
        let ok = [comp("a", State::Operational)];
        let s = say(&ok);
        assert_eq!(flat(&s.headline), "Everything is up.");
        assert_eq!(flat(&s.detail), "Nothing has broken yet.");

        let s = status_say(&StatusFacts {
            components: &ok,
            last_resolved: Some(NOW - 12 * 86_400 - 60),
            now: NOW,
            ..Default::default()
        });
        assert_eq!(flat(&s.detail), "Nothing has broken in 12 days.");
        let s = status_say(&StatusFacts {
            components: &ok,
            last_resolved: Some(NOW - 86_400),
            now: NOW,
            ..Default::default()
        });
        assert_eq!(flat(&s.detail), "Nothing has broken in 1 day.");
        let s = status_say(&StatusFacts {
            components: &ok,
            last_resolved: Some(NOW - 600),
            now: NOW,
            ..Default::default()
        });
        assert_eq!(flat(&s.detail), "Nothing has broken since earlier today.");
    }

    #[test]
    fn upcoming_maintenance_and_empty_config() {
        let ok = [comp("a", State::Operational)];
        let up = [Upcoming {
            component: "a".into(),
            starts: NOW + 3600,
        }];
        let s = status_say(&StatusFacts {
            components: &ok,
            upcoming: &up,
            now: NOW,
            ..Default::default()
        });
        assert!(flat(&s.detail).ends_with("Maintenance on a starts 2025-09-24 08:46 UTC."));
        assert_eq!(flat(&say(&[]).headline), "Nothing to watch yet.");
    }

    #[test]
    fn resources_run_high_rather_than_slow() {
        let mut disk = comp("Root disk", State::Degraded);
        disk.resource = true;
        assert_eq!(
            flat(&say(&[disk.clone()]).headline),
            "*Root disk* is running high."
        );
        let mut down = comp("api", State::MajorOutage);
        down.since = Some(NOW - 90);
        let s = say(&[down, disk]);
        assert_eq!(flat(&s.detail), "Root disk is running high.");
    }

    #[test]
    fn numbers_and_latency() {
        assert_eq!(number_word(5), "five");
        assert_eq!(number_word(13), "13");
        assert_eq!(fmt_latency(84.2), "84 ms");
        assert_eq!(fmt_latency(1400.0), "1.4 s");
    }

    #[test]
    fn host_sentence() {
        let m = |kind, value| HostMetric {
            kind,
            value,
            warn: 90.0,
        };
        let fine = [
            m(HostKind::Cpu, 12.3),
            m(HostKind::Memory, 41.0),
            m(HostKind::Disk("/".into()), 68.4),
        ];
        assert_eq!(
            flat(&host_say(&fine).headline),
            "The server is fine: 12% CPU, 41% memory, disk 68% full."
        );
        let busy = [
            m(HostKind::Cpu, 95.0),
            m(HostKind::Memory, 93.0),
            m(HostKind::Disk("/".into()), 68.0),
            m(HostKind::Disk("/data".into()), 20.0),
        ];
        let s = host_say(&busy);
        assert_eq!(flat(&s.headline), "The *CPU* is busy: 95%.");
        assert_eq!(
            flat(&s.detail),
            "Trouble starts at 90%. The rest: 93% memory, disk / 68% full, disk /data 20% full."
        );
        let disk = [m(HostKind::Disk("/".into()), 97.0), m(HostKind::Cpu, 5.0)];
        assert_eq!(
            flat(&host_say(&disk).headline),
            "*Disk /* is almost full: 97% used."
        );
        assert_eq!(
            flat(&host_say(&[]).headline),
            "No numbers from the server yet."
        );
    }
}
