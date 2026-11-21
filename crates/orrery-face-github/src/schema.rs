//! GitHub's check of a request body against the schema it keeps for a
//! route. It names every fault it finds, one to a line, in the order its
//! checker walks a schema, and in that checker's words. A schema here is
//! what GitHub was seen to check, which is not always what it publishes.

use axum::body::Body;
use axum::http::Response;

use crate::invalid_request;
use crate::sent::Sent;

/// The kinds of value a schema asks for.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    String,
    Array,
    Object,
    ObjectOrNull,
}

/// What GitHub checks of one value.
pub(crate) struct Schema {
    /// Schemas of which the value must pass one.
    pub any_of: &'static [Schema],
    /// The strings the value must be one of, when there are any.
    pub members: &'static [&'static str],
    pub kind: Option<Kind>,
    /// What each item of an array is checked against.
    pub items: Option<&'static Schema>,
    pub max_items: Option<usize>,
    /// An object's members, in the order GitHub checks them.
    pub properties: &'static [(&'static str, Schema)],
    /// The members an object must have.
    pub required: &'static [&'static str],
    /// Whether a string is a time.
    pub date_time: bool,
    /// The most characters a string may have.
    pub max_length: Option<usize>,
}

impl Schema {
    /// A schema that asks nothing.
    pub(crate) const ANY: Schema = Schema {
        any_of: &[],
        members: &[],
        kind: None,
        items: None,
        max_items: None,
        properties: &[],
        required: &[],
        date_time: false,
        max_length: None,
    };

    pub(crate) const STRING: Schema = Schema {
        kind: Some(Kind::String),
        ..Schema::ANY
    };

    /// GitHub's `422` for a body the schema refuses, or nothing for one it
    /// takes. `name` is what GitHub calls the schema in a fault of the
    /// body's own type, as `links/0/schema`.
    pub(crate) fn refuse(&self, body: &Sent, name: &str) -> Option<Response<Body>> {
        let mut faults = Vec::new();
        let mut of_branches = Vec::new();
        self.check(body, name, &mut faults, &mut of_branches);
        // The faults of the schemas none of which matched come last.
        faults.extend(of_branches);
        match faults.is_empty() {
            true => None,
            false => Some(invalid_request(&faults.join("\n"))),
        }
    }

    /// The checks, in the order GitHub's checker makes them: one-of-these,
    /// membership, type, then what applies to an array, an object, or a
    /// string.
    fn check(&self, value: &Sent, name: &str, faults: &mut Vec<String>, later: &mut Vec<String>) {
        if !self.any_of.is_empty() {
            let mut of_all = Vec::new();
            let mut matched = false;
            for branch in self.any_of {
                let mut own = Vec::new();
                let mut nested = Vec::new();
                branch.check(value, name, &mut own, &mut nested);
                own.extend(nested);
                matched |= own.is_empty();
                of_all.extend(own);
            }
            if !matched {
                faults.push("No subschema in \"anyOf\" matched.".to_string());
                later.extend(of_all);
            }
        }
        if !self.members.is_empty()
            && !matches!(value, Sent::String(text) if self.members.contains(&text.as_str()))
        {
            let members: Vec<String> = self.members.iter().map(|m| format!("\"{m}\"")).collect();
            faults.push(format!(
                "{} is not a member of [{}].",
                value.said(),
                members.join(", ")
            ));
        }
        let wanted = match (self.kind, value) {
            (Some(Kind::String), Sent::String(_))
            | (Some(Kind::Array), Sent::Array(_))
            | (Some(Kind::Object | Kind::ObjectOrNull), Sent::Object(_))
            | (Some(Kind::ObjectOrNull), Sent::Null)
            | (None, _) => None,
            (Some(Kind::String), _) => Some("a string"),
            (Some(Kind::Array), _) => Some("an array"),
            (Some(Kind::Object), _) => Some("an object"),
            (Some(Kind::ObjectOrNull), _) => Some("an object or null"),
        };
        if let Some(wanted) = wanted {
            faults.push(format!(
                "For '{name}', {} is not {wanted}.",
                value.spelled()
            ));
        }
        match value {
            Sent::Array(items) => {
                if let Some(schema) = self.items {
                    for item in items {
                        schema.check(item, "items", faults, later);
                    }
                }
                if let Some(most) = self.max_items.filter(|most| items.len() > *most) {
                    faults.push(format!(
                        "No more than {most} items are allowed; {} were supplied.",
                        items.len()
                    ));
                }
            }
            Sent::Object(_) => {
                for (key, schema) in self.properties {
                    if let Some(member) = value.member(key) {
                        schema.check(member, &format!("properties/{key}"), faults, later);
                    }
                }
                let mut missing: Vec<&str> = self
                    .required
                    .iter()
                    .copied()
                    .filter(|key| value.member(key).is_none())
                    .collect();
                missing.sort_unstable();
                let quoted: Vec<String> = missing.iter().map(|key| format!("\"{key}\"")).collect();
                match quoted.len() {
                    0 => {}
                    1 => faults.push(format!("{} wasn't supplied.", quoted[0])),
                    _ => faults.push(format!("{} weren't supplied.", quoted.join(", "))),
                }
            }
            Sent::String(text) => {
                if self.date_time && !is_date_time(text) {
                    faults.push(format!("{text} is not a valid date-time."));
                }
                let length = text.chars().count();
                if let Some(most) = self.max_length.filter(|most| length > *most) {
                    faults.push(format!(
                        "Only {most} characters are allowed; {length} were supplied."
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Whether GitHub's schema takes the text as a time: a date, `T`, a time
/// of day whose minutes and seconds are below 60, perhaps a fraction, and
/// `Z` or an offset whose minutes are below 60. An hour, a day, or a month
/// that no clock or calendar has still passes.
pub(crate) fn is_date_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digit = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    let below_six = |at: usize| bytes.get(at).is_some_and(|b| (b'0'..=b'5').contains(b));
    let is = |at: usize, byte: u8| bytes.get(at) == Some(&byte);
    let shape = (0..4).all(digit)
        && is(4, b'-')
        && digit(5)
        && digit(6)
        && is(7, b'-')
        && digit(8)
        && digit(9)
        && is(10, b'T')
        && digit(11)
        && digit(12)
        && is(13, b':')
        && below_six(14)
        && digit(15)
        && is(16, b':')
        && below_six(17)
        && digit(18);
    if !shape {
        return false;
    }
    let mut at = 19;
    if is(at, b'.') {
        at += 1;
        let from = at;
        while digit(at) {
            at += 1;
        }
        if at == from {
            return false;
        }
    }
    match bytes.get(at) {
        Some(b'Z') => at + 1 == bytes.len(),
        Some(b'+' | b'-') => {
            digit(at + 1)
                && digit(at + 2)
                && is(at + 3, b':')
                && below_six(at + 4)
                && digit(at + 5)
                && at + 6 == bytes.len()
        }
        _ => false,
    }
}

/// The time a text that passed [`is_date_time`] names, in seconds since
/// 1970, or `None` for one GitHub fails to read: a month that is none of
/// the twelve, a day below 1 or above 31, an hour above 24, an hour of 24
/// with a minute or a second, and an offset of a day or more. A day a
/// month does not have runs into the next month, as an hour of 24 runs
/// into the next day. A fraction of a second is dropped.
pub(crate) fn instant(text: &str) -> Option<i64> {
    let number = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let zone = text[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone.as_bytes().first()? {
        b'Z' => 0,
        sign => {
            let hours = zone.get(1..3)?.parse::<i64>().ok()?;
            let minutes = zone.get(4..6)?.parse::<i64>().ok()?;
            let seconds = hours * 3600 + minutes * 60;
            if seconds >= 86_400 {
                return None;
            }
            match sign {
                b'-' => -seconds,
                _ => seconds,
            }
        }
    };
    let real = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && (hour < 24 || (hour == 24 && minute == 0 && second == 0));
    if !real {
        return None;
    }
    Some(
        (days_before(year, month) + day - 1) * 86_400 + hour * 3600 + minute * 60 + second - offset,
    )
}

/// The days from 1970-01-01 to the first of a month.
fn days_before(year: i64, month: i64) -> i64 {
    // Counting years from March puts the leap day last.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year.rem_euclid(400);
    let of_year = (153 * ((month + 9) % 12) + 2) / 5;
    era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each as github.com answered it as a check run's `started_at` on
    /// 2026-11-21: `422` for a text its schema refuses, `500` for one it
    /// fails to read, and otherwise the time it answered back.
    #[test]
    fn a_time_is_read_as_github_reads_it() {
        const T: i64 = 1_767_323_045; // 2026-01-02T03:04:05Z
        for refused in [
            "x",
            "",
            "2026-01-02",
            "2026-01-02 03:04:05",
            "2026-01-02T03:04:05",
            "2026-01-02t03:04:05z",
            "2026-01-02T03:60:05Z",
            "2026-01-02T03:04:60Z",
            "2026-01-02T03:04:61Z",
            " 2026-01-02T03:04:05Z",
            "2026-01-02T03:04:05+02:99",
            "2026-01-02T03:04:05+23:99",
            "2026-01-02T03:04:05.Z",
        ] {
            assert!(!is_date_time(refused), "{refused}");
        }
        for (failed, _) in [
            ("2026-13-02T03:04:05Z", "month 13"),
            ("2026-01-32T03:04:05Z", "day 32"),
            ("2026-01-00T03:04:05Z", "day 00"),
            ("2026-00-10T03:04:05Z", "month 00"),
            ("2026-01-02T25:00:00Z", "hour 25"),
            ("2026-01-02T24:30:00Z", "hour 24 and a half"),
            ("2026-01-02T24:00:01Z", "hour 24 and a second"),
            ("2026-01-02T03:04:05+25:00", "offset 25"),
            ("2026-01-02T03:04:05+24:00", "offset 24"),
        ] {
            assert!(is_date_time(failed), "{failed}");
            assert_eq!(instant(failed), None, "{failed}");
        }
        for (text, read) in [
            ("2026-01-02T03:04:05Z", T),
            ("2026-01-02T03:04:05+02:00", T - 7200),
            ("2026-01-02T03:04:05.678Z", T),
            ("2026-01-02T03:04:05.123456789Z", T),
            ("2026-01-02T03:04:05-00:00", T),
            // 2026-01-03T03:03:05Z
            ("2026-01-02T03:04:05-23:59", T + 86_340),
            // 2026-03-02T03:04:05Z
            ("2026-02-30T03:04:05Z", T + 59 * 86_400),
            // 2025-03-01T03:04:05Z
            ("2025-02-29T03:04:05Z", T - 307 * 86_400),
            // 2026-05-01T03:04:05Z
            ("2026-04-31T03:04:05Z", T + 119 * 86_400),
            // 2026-01-03T00:00:00Z
            ("2026-01-02T24:00:00Z", T + 86_400 - 11_045),
            ("1970-01-01T00:00:00Z", 0),
            ("0000-01-02T03:04:05Z", -62_167_121_755),
            ("9999-12-31T23:59:59Z", 253_402_300_799),
        ] {
            assert!(is_date_time(text), "{text}");
            assert_eq!(instant(text), Some(read), "{text}");
        }
        let answered = |text: &str| crate::render::rfc3339(instant(text).unwrap());
        for (text, back) in [
            ("2026-01-02T03:04:05.678Z", "2026-01-02T03:04:05Z"),
            ("2026-01-02T03:04:05+02:00", "2026-01-02T01:04:05Z"),
            ("2026-01-02T03:04:05-23:59", "2026-01-03T03:03:05Z"),
            ("2026-02-30T03:04:05Z", "2026-03-02T03:04:05Z"),
            ("2025-02-29T03:04:05Z", "2025-03-01T03:04:05Z"),
            ("2026-04-31T03:04:05Z", "2026-05-01T03:04:05Z"),
            ("2026-01-02T24:00:00Z", "2026-01-03T00:00:00Z"),
            ("2024-02-29T23:59:59Z", "2024-02-29T23:59:59Z"),
            ("1969-12-31T23:59:59Z", "1969-12-31T23:59:59Z"),
            ("0000-01-02T03:04:05Z", "0000-01-02T03:04:05Z"),
            ("9999-12-31T23:59:59Z", "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(answered(text), back, "{text}");
        }
    }
}
