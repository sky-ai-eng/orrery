//! A request body as GitHub reads one. GitHub's parser takes more than JSON
//! allows and answers something for a body that stops part way, and its
//! messages spell a value as Ruby prints it, so the face reads a body the
//! same way wherever what was sent shows in the answer.

use std::borrow::Cow;
use std::collections::BTreeMap;

/// A JSON value as a request sent it. An object's members stay in the order
/// they came in, which is the order GitHub's messages spell them in, and an
/// integer keeps every digit it was sent with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Sent {
    Null,
    Bool(bool),
    /// An integer's digits, with a leading `-` for one below zero.
    Integer(String),
    Float(f64),
    String(String),
    Array(Vec<Sent>),
    Object(Vec<(String, Sent)>),
}

/// A body GitHub answers `400` "Problems parsing JSON" to.
#[derive(Debug, PartialEq)]
pub(crate) struct NotJson;

static NOTHING: Sent = Sent::Null;

/// How deep a body's arrays and objects may nest, which is as deep as the
/// strict readers take one. A body past it is refused as one that is not
/// JSON, so that no reader of a value can exhaust the stack.
const MAX_DEPTH: usize = 127;

impl Sent {
    /// Read a body as GitHub's parser does. It allows comments. A body that
    /// ends part way is no fault: the answer is whatever was open when it
    /// ended, which is the innermost array or object, or the name of the
    /// member whose value never came. A body that holds nothing is null.
    pub(crate) fn parse(body: &[u8]) -> Result<Sent, NotJson> {
        Reader::new(body).read()
    }

    /// The body a request sent, or null for one GitHub cannot parse.
    pub(crate) fn of(body: &[u8]) -> Sent {
        Sent::parse(body).unwrap_or(Sent::Null)
    }

    /// An object's member of this name, when the value is an object that
    /// has one.
    pub(crate) fn member(&self, key: &str) -> Option<&Sent> {
        match self {
            Sent::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// The member of this name, or null where there is none.
    pub(crate) fn at(&self, key: &str) -> &Sent {
        self.member(key).unwrap_or(&NOTHING)
    }

    /// An array's item at this place, or null where there is none.
    pub(crate) fn item(&self, index: usize) -> &Sent {
        match self {
            Sent::Array(items) => items.get(index).unwrap_or(&NOTHING),
            _ => &NOTHING,
        }
    }

    /// The text of a string.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Sent::String(text) => Some(text),
            _ => None,
        }
    }

    /// Whether GitHub reads the value as nothing: null, false, a string of
    /// nothing but spaces, and an array or an object that holds nothing.
    pub(crate) fn is_blank(&self) -> bool {
        match self {
            Sent::Null | Sent::Bool(false) => true,
            Sent::String(text) => text.trim().is_empty(),
            Sent::Array(items) => items.is_empty(),
            Sent::Object(members) => members.is_empty(),
            Sent::Bool(true) | Sent::Integer(_) | Sent::Float(_) => false,
        }
    }

    /// The value as GitHub's messages spell it, which is as Ruby prints
    /// one: JSON, but `nil` for null, an object's members as
    /// `"key" => value`, and a number with a fraction as Ruby writes it.
    pub(crate) fn spelled(&self) -> String {
        match self {
            Sent::Null => "nil".to_string(),
            Sent::Bool(value) => value.to_string(),
            Sent::Integer(digits) => digits.clone(),
            Sent::Float(value) => ruby_float(*value),
            Sent::String(value) => serde_json::Value::from(value.as_str()).to_string(),
            Sent::Array(items) => {
                let items: Vec<String> = items.iter().map(Sent::spelled).collect();
                format!("[{}]", items.join(", "))
            }
            Sent::Object(members) => {
                let members: Vec<String> = members
                    .iter()
                    .map(|(key, value)| {
                        format!(
                            "{} => {}",
                            serde_json::Value::from(key.as_str()),
                            value.spelled()
                        )
                    })
                    .collect();
                format!("{{{}}}", members.join(", "))
            }
        }
    }

    /// The value as GitHub writes it into a sentence, which is as
    /// [`Sent::spelled`] but a string without its quotes and null as
    /// nothing at all.
    pub(crate) fn said(&self) -> String {
        match self {
            Sent::Null => String::new(),
            Sent::String(text) => text.clone(),
            other => other.spelled(),
        }
    }

    /// An integer's value, held to what 64 bits can.
    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Sent::Integer(digits) => {
                Some(digits.parse().unwrap_or(match digits.starts_with('-') {
                    true => i64::MIN,
                    false => i64::MAX,
                }))
            }
            _ => None,
        }
    }

    /// The value as JSON, with its members in the order they were sent.
    fn write_json(&self, out: &mut String) {
        match self {
            Sent::Null => out.push_str("null"),
            Sent::Bool(value) => out.push_str(&value.to_string()),
            Sent::Integer(digits) => out.push_str(digits),
            // JSON has no infinity, and a number past what a float holds
            // reads back as one.
            Sent::Float(value) if value.is_infinite() => {
                out.push_str(if *value > 0.0 { "1e999" } else { "-1e999" });
            }
            Sent::Float(value) => out.push_str(
                &serde_json::Number::from_f64(*value)
                    .map_or_else(|| "null".to_string(), |n| n.to_string()),
            ),
            Sent::String(text) => out.push_str(&serde_json::Value::from(text.as_str()).to_string()),
            Sent::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            Sent::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::from(key.as_str()).to_string());
                    out.push(':');
                    value.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// A body as a strict JSON reader takes what GitHub read from it: the same
/// bytes for a body GitHub cannot parse, which the reader then refuses as
/// GitHub does, and for a body that holds nothing, and otherwise what
/// GitHub read, written as JSON.
pub(crate) fn as_github_reads(body: &[u8]) -> Cow<'_, [u8]> {
    match Sent::parse(body) {
        Err(NotJson) => Cow::Borrowed(body),
        Ok(Sent::Null) if body.iter().all(u8::is_ascii_whitespace) => Cow::Borrowed(body),
        Ok(Sent::Null) => Cow::Borrowed(b"null"),
        Ok(read) => {
            let mut json = String::new();
            read.write_json(&mut json);
            Cow::Owned(json.into_bytes())
        }
    }
}

/// A float as Ruby writes one: the shortest digits that read back as it,
/// with a point always, and an exponent for one of 10^15 or more or below
/// 10^-4.
fn ruby_float(value: f64) -> String {
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // Rust's `{:e}` is the shortest digits too, as `d.ddde±x`.
    let written = format!("{:e}", value.abs());
    let (mantissa, exponent) = written.split_once('e').unwrap_or((&written, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    // Where the point falls, counted in digits from the first.
    let point = exponent + 1;
    if point > -4 && point <= 15 {
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else if digits.len() <= point as usize {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        return format!("{sign}{body}");
    }
    let rest = match digits.len() {
        1 => "0",
        _ => &digits[1..],
    };
    let mark = if exponent < 0 { '-' } else { '+' };
    format!(
        "{sign}{}.{rest}e{mark}{:02}",
        &digits[..1],
        exponent.unsigned_abs()
    )
}

/// What the reader has open: an array, an object, or the name of an
/// object's member whose value has not come.
enum Open {
    Array(Vec<Sent>),
    /// An object's members, and where each name is among them.
    Object(Vec<(String, Sent)>, BTreeMap<String, usize>),
    Key(String),
    /// A value at the top with nothing around it.
    Whole(Sent),
}

/// What may come next.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// A value at the top.
    Start,
    /// Nothing: the top value is whole.
    End,
    /// A member's name, or the object's end.
    FirstKey,
    /// A member's name.
    Key,
    Colon,
    /// A member's value.
    MemberValue,
    /// A comma, or the object's end.
    AfterMember,
    /// An item, or the array's end.
    FirstItem,
    /// An item.
    Item,
    /// A comma, or the array's end.
    AfterItem,
}

enum Token {
    OpenObject,
    CloseObject,
    OpenArray,
    CloseArray,
    Comma,
    Colon,
    Value(Sent),
    /// The body ended, perhaps inside a token, which is then as if it had
    /// not been sent.
    End,
}

struct Reader<'a> {
    body: &'a [u8],
    at: usize,
    open: Vec<Open>,
    /// How many arrays and objects are open.
    depth: usize,
    expect: Expect,
}

impl<'a> Reader<'a> {
    fn new(body: &'a [u8]) -> Reader<'a> {
        Reader {
            body,
            at: 0,
            open: Vec::new(),
            depth: 0,
            expect: Expect::Start,
        }
    }

    fn read(mut self) -> Result<Sent, NotJson> {
        loop {
            let token = self.token()?;
            if matches!(token, Token::End) {
                break;
            }
            self.take(token)?;
        }
        // GitHub answers what is on top of what was open.
        Ok(match self.open.pop() {
            None => Sent::Null,
            Some(Open::Array(items)) => Sent::Array(items),
            Some(Open::Object(members, _)) => Sent::Object(members),
            Some(Open::Key(name)) => Sent::String(name),
            Some(Open::Whole(value)) => value,
        })
    }

    /// Take one token where it is allowed, or refuse the body.
    fn take(&mut self, token: Token) -> Result<(), NotJson> {
        use Expect::*;
        match (self.expect, token) {
            (Start | MemberValue | FirstItem | Item, Token::OpenObject) => {
                self.descend(Open::Object(Vec::new(), BTreeMap::new()), FirstKey)
            }
            (Start | MemberValue | FirstItem | Item, Token::OpenArray) => {
                self.descend(Open::Array(Vec::new()), FirstItem)
            }
            (Start | MemberValue | FirstItem | Item, Token::Value(value)) => {
                self.place(value);
                Ok(())
            }
            (FirstItem | AfterItem, Token::CloseArray)
            | (FirstKey | AfterMember, Token::CloseObject) => {
                self.close();
                Ok(())
            }
            (FirstKey | Key, Token::Value(Sent::String(name))) => {
                self.open.push(Open::Key(name));
                self.expect = Colon;
                Ok(())
            }
            (Colon, Token::Colon) => {
                self.expect = MemberValue;
                Ok(())
            }
            (AfterMember, Token::Comma) => {
                self.expect = Key;
                Ok(())
            }
            (AfterItem, Token::Comma) => {
                self.expect = Item;
                Ok(())
            }
            _ => Err(NotJson),
        }
    }

    fn descend(&mut self, opened: Open, expect: Expect) -> Result<(), NotJson> {
        if self.depth >= MAX_DEPTH {
            return Err(NotJson);
        }
        self.depth += 1;
        self.open.push(opened);
        self.expect = expect;
        Ok(())
    }

    /// Put a whole value where it belongs in what is open.
    fn place(&mut self, value: Sent) {
        match self.open.pop() {
            None => {
                self.open.push(Open::Whole(value));
                self.expect = Expect::End;
            }
            Some(Open::Array(mut items)) => {
                items.push(value);
                self.open.push(Open::Array(items));
                self.expect = Expect::AfterItem;
            }
            Some(Open::Key(name)) => {
                if let Some(Open::Object(members, places)) = self.open.last_mut() {
                    // A name sent twice keeps its first place and its last
                    // value.
                    match places.get(&name) {
                        Some(place) => members[*place].1 = value,
                        None => {
                            places.insert(name.clone(), members.len());
                            members.push((name, value));
                        }
                    }
                }
                self.expect = Expect::AfterMember;
            }
            Some(other) => self.open.push(other),
        }
    }

    /// Close the array or object on top and put it where it belongs.
    fn close(&mut self) {
        let closed = match self.open.pop() {
            Some(Open::Array(items)) => Sent::Array(items),
            Some(Open::Object(members, _)) => Sent::Object(members),
            _ => return,
        };
        self.depth -= 1;
        self.place(closed);
    }

    fn token(&mut self) -> Result<Token, NotJson> {
        loop {
            let Some(&byte) = self.body.get(self.at) else {
                return Ok(Token::End);
            };
            match byte {
                b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' => self.at += 1,
                b'/' => {
                    if !self.comment()? {
                        return Ok(Token::End);
                    }
                }
                _ => break,
            }
        }
        let byte = self.body[self.at];
        let simple = match byte {
            b'{' => Some(Token::OpenObject),
            b'}' => Some(Token::CloseObject),
            b'[' => Some(Token::OpenArray),
            b']' => Some(Token::CloseArray),
            b',' => Some(Token::Comma),
            b':' => Some(Token::Colon),
            _ => None,
        };
        if let Some(token) = simple {
            self.at += 1;
            return Ok(token);
        }
        match byte {
            b'"' => self.string(),
            b'-' | b'0'..=b'9' => self.number(),
            b't' => self.word(b"true", Sent::Bool(true)),
            b'f' => self.word(b"false", Sent::Bool(false)),
            b'n' => self.word(b"null", Sent::Null),
            _ => Err(NotJson),
        }
    }

    /// Pass a comment. `false` when the body ends inside it.
    fn comment(&mut self) -> Result<bool, NotJson> {
        match self.body.get(self.at + 1) {
            None => {
                self.at = self.body.len();
                Ok(false)
            }
            Some(b'/') => {
                match self.body[self.at..].iter().position(|b| *b == b'\n') {
                    Some(end) => self.at += end + 1,
                    None => self.at = self.body.len(),
                }
                Ok(true)
            }
            Some(b'*') => {
                let rest = &self.body[self.at + 2..];
                match rest.windows(2).position(|pair| pair == b"*/") {
                    Some(end) => {
                        self.at += 2 + end + 2;
                        Ok(true)
                    }
                    None => {
                        self.at = self.body.len();
                        Ok(false)
                    }
                }
            }
            Some(_) => Err(NotJson),
        }
    }

    fn word(&mut self, word: &[u8], value: Sent) -> Result<Token, NotJson> {
        let rest = &self.body[self.at..];
        if rest.len() < word.len() {
            // The body ends inside the word, if it is the word so far.
            return match word.starts_with(rest) {
                true => {
                    self.at = self.body.len();
                    Ok(Token::End)
                }
                false => Err(NotJson),
            };
        }
        if &rest[..word.len()] != word {
            return Err(NotJson);
        }
        self.at += word.len();
        Ok(Token::Value(value))
    }

    fn number(&mut self) -> Result<Token, NotJson> {
        let start = self.at;
        let mut at = self.at;
        let digits = |at: &mut usize| {
            let from = *at;
            while self.body.get(*at).is_some_and(u8::is_ascii_digit) {
                *at += 1;
            }
            *at - from
        };
        // A number the body ends inside is as if it had not been sent.
        let ended = |at: usize| at >= self.body.len();
        if self.body[at] == b'-' {
            at += 1;
        }
        match self.body.get(at) {
            None => {
                self.at = self.body.len();
                return Ok(Token::End);
            }
            Some(b'0') => at += 1,
            Some(b'1'..=b'9') => {
                digits(&mut at);
            }
            Some(_) => return Err(NotJson),
        }
        let mut whole = true;
        if self.body.get(at) == Some(&b'.') {
            whole = false;
            at += 1;
            if digits(&mut at) == 0 {
                return match ended(at) {
                    true => {
                        self.at = self.body.len();
                        Ok(Token::End)
                    }
                    false => Err(NotJson),
                };
            }
        }
        if matches!(self.body.get(at), Some(b'e' | b'E')) {
            whole = false;
            at += 1;
            if matches!(self.body.get(at), Some(b'+' | b'-')) {
                at += 1;
            }
            if digits(&mut at) == 0 {
                return match ended(at) {
                    true => {
                        self.at = self.body.len();
                        Ok(Token::End)
                    }
                    false => Err(NotJson),
                };
            }
        }
        let text = std::str::from_utf8(&self.body[start..at]).map_err(|_| NotJson)?;
        self.at = at;
        Ok(Token::Value(match whole {
            true => Sent::Integer(match text.trim_start_matches('-') {
                "0" => "0".to_string(),
                _ => text.to_string(),
            }),
            false => Sent::Float(text.parse().map_err(|_| NotJson)?),
        }))
    }

    fn string(&mut self) -> Result<Token, NotJson> {
        let mut at = self.at + 1;
        let mut text: Vec<u8> = Vec::new();
        let ends = |reader: &mut Reader<'_>| {
            reader.at = reader.body.len();
            Ok(Token::End)
        };
        loop {
            let Some(&byte) = self.body.get(at) else {
                return ends(self);
            };
            at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escaped) = self.body.get(at) else {
                        return ends(self);
                    };
                    at += 1;
                    match escaped {
                        b'"' | b'\\' | b'/' => text.push(escaped),
                        b'b' => text.push(0x08),
                        b'f' => text.push(0x0c),
                        b'n' => text.push(b'\n'),
                        b'r' => text.push(b'\r'),
                        b't' => text.push(b'\t'),
                        b'u' => {
                            let Some(unit) = self.hex4(at)? else {
                                return ends(self);
                            };
                            at += 4;
                            let mut point = unit;
                            if unit & 0xFC00 == 0xD800 {
                                // The first half of a pair is joined to the
                                // escape after it, and is `?` with none.
                                if self.body.get(at) == Some(&b'\\')
                                    && self.body.get(at + 1) == Some(&b'u')
                                {
                                    let Some(second) = self.hex4(at + 2)? else {
                                        return ends(self);
                                    };
                                    at += 6;
                                    point = ((unit & 0x3F) << 10)
                                        | ((((unit >> 6) & 0xF) + 1) << 16)
                                        | (second & 0x3FF);
                                } else {
                                    point = u32::from(b'?');
                                }
                            }
                            let letter = char::from_u32(point).unwrap_or('?');
                            text.extend_from_slice(letter.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        _ => return Err(NotJson),
                    }
                }
                0x00..=0x1f => return Err(NotJson),
                _ => text.push(byte),
            }
        }
        self.at = at;
        String::from_utf8(text)
            .map(|text| Token::Value(Sent::String(text)))
            .map_err(|_| NotJson)
    }

    /// Four hex digits at `at`, or `None` when the body ends before them.
    fn hex4(&self, at: usize) -> Result<Option<u32>, NotJson> {
        let mut value = 0;
        for i in 0..4 {
            let Some(&byte) = self.body.get(at + i) else {
                return Ok(None);
            };
            let digit = (byte as char).to_digit(16).ok_or(NotJson)?;
            value = value * 16 + digit;
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelled(body: &str) -> String {
        Sent::parse(body.as_bytes()).unwrap().spelled()
    }

    #[test]
    fn a_value_is_spelled_as_ruby_prints_it_in_the_order_it_was_sent() {
        let sent = Sent::of(
            br#"{"zeta": 1, "alpha": [1, "b", null], "mid": {"y": true, "b": 1.5}, "zeta": 2}"#,
        );
        assert_eq!(
            sent.spelled(),
            r#"{"zeta" => 2, "alpha" => [1, "b", nil], "mid" => {"y" => true, "b" => 1.5}}"#
        );
        assert_eq!(sent.at("alpha").item(1).spelled(), r#""b""#);
        assert_eq!(sent.at("alpha").item(9).spelled(), "nil");
        assert_eq!(sent.at("absent").spelled(), "nil");
        assert_eq!(Sent::of(b"nonsense").spelled(), "nil");
    }

    #[test]
    fn nothing_is_what_github_reads_as_nothing() {
        for blank in ["null", "false", r#""""#, r#"" \t ""#, "[]", "{}"] {
            assert!(Sent::of(blank.as_bytes()).is_blank(), "{blank}");
        }
        for present in ["true", "0", r#"" x ""#, "[null]", r#"{"a": null}"#] {
            assert!(!Sent::of(present.as_bytes()).is_blank(), "{present}");
        }
    }

    /// Each as github.com spelled it in the fault of a field that takes a
    /// string, on 2026-11-21.
    #[test]
    fn a_number_is_spelled_as_github_spells_it() {
        for (sent, spelling) in [
            ("1e5", "100000.0"),
            ("1E+2", "100.0"),
            ("-0", "0"),
            ("-0.0", "-0.0"),
            ("1.50", "1.5"),
            ("12345678901234567890123", "12345678901234567890123"),
            ("1e400", "Infinity"),
            ("1.0e-7", "1.0e-07"),
            ("123456789012345678.0", "1.2345678901234568e+17"),
            ("1e16", "1.0e+16"),
            ("1e15", "1.0e+15"),
            ("0.0001", "0.0001"),
            ("0.00001", "1.0e-05"),
            ("2.5e-5", "2.5e-05"),
            (
                r#"[1, 2.0, "a", null, true, {"k": [ ]}]"#,
                r#"[1, 2.0, "a", nil, true, {"k" => []}]"#,
            ),
        ] {
            assert_eq!(spelled(sent), spelling, "{sent}");
        }
    }

    /// Each as github.com answered it on 2026-11-21: `400` for the ones
    /// refused here.
    #[test]
    fn a_body_github_cannot_parse_is_refused() {
        for body in [
            "nonsense",
            r#"{"a": 1}}"#,
            r#"{"a": 1,}"#,
            r#"{"a": 1} x"#,
            r#"{"a": 1}{"b": 2}"#,
            r#"{"a": 1}]"#,
            "\u{feff}{}",
            "{\"a\": \"tab\there\"}",
            r#"{"a": NaN}"#,
            r#"{"a": 'x'}"#,
            r#"{"a": "x" "b": 1}"#,
            r#"{"a" "x"}"#,
            r#"{"a": 01}"#,
            r#"{"a": "x"]"#,
            r#"{"a": .5}"#,
            r#"{"a": +1}"#,
            r#"{"a": 1.}"#,
            r#"{"a": Infinity}"#,
            r#"{"a": "\q"}"#,
            "{\"a\": \"\u{1f}\"}",
        ] {
            assert_eq!(Sent::parse(body.as_bytes()), Err(NotJson), "{body}");
        }
        assert_eq!(Sent::parse(b"\xff\xfe"), Err(NotJson));
        assert_eq!(Sent::parse(b"{\"a\": \"a\xffb\"}"), Err(NotJson));
    }

    /// Each as github.com read it on 2026-11-21, seen in the fault it then
    /// answered or in what it kept.
    #[test]
    fn a_body_that_stops_part_way_is_what_was_open_when_it_stopped() {
        for (body, read) in [
            ("", "nil"),
            ("   ", "nil"),
            ("null", "nil"),
            ("{", "{}"),
            ("[", "[]"),
            ("[1,", "[1]"),
            (r#"{"name": "q""#, r#"{"name" => "q"}"#),
            (r#"{"name": "q","#, r#"{"name" => "q"}"#),
            (r#"{"name": "q", "ext"#, r#"{"name" => "q"}"#),
            (r#"{"name": "q", "external_id""#, r#""external_id""#),
            (r#"{"name": "q", "external_id":"#, r#""external_id""#),
            (r#"{"name": "q", "external_id": "abc"#, r#""external_id""#),
            (r#"{"name": "q", "external_id": "a\"#, r#""external_id""#),
            (r#"{"name": "q", "external_id": "a\u00"#, r#""external_id""#),
            (r#"{"name": "q", "status": tru"#, r#""status""#),
            (r#"{"name": "q", "output": nul"#, r#""output""#),
            (
                r#"{"name": "q", "output": {"annotations": [{"path": "a", "start_line": 12"#,
                r#"{"path" => "a", "start_line" => 12}"#,
            ),
            (
                r#"{"name": "q", "output": {"annotations": [{"path": "a", "start_line": 1."#,
                r#""start_line""#,
            ),
            (
                r#"{"name": "q", "output": {"annotations": [{"path": "a", "start_line": -"#,
                r#""start_line""#,
            ),
            (r#""abc"#, "nil"),
            ("tru", "nil"),
            ("-", "nil"),
            ("12", "12"),
            ("true", "true"),
            (r#"{"a": 1} "#, r#"{"a" => 1}"#),
            (r#"{"a": 1, /* c */ "b": 2}"#, r#"{"a" => 1, "b" => 2}"#),
            ("{\"a\": 1, // c\n \"b\": 2}", r#"{"a" => 1, "b" => 2}"#),
            (r#"{"a": 1 /* c"#, r#"{"a" => 1}"#),
            (r#"{"a": "x", "a": 5}"#, r#"{"a" => 5}"#),
        ] {
            assert_eq!(spelled(body), read, "{body}");
        }
    }

    #[test]
    fn a_string_is_read_as_github_reads_it() {
        for (body, text) in [
            (r#""é\n\t\"\\\/""#, "é\n\t\"\\/"),
            (r#""😀""#, "😀"),
            (r#""\ud800""#, "?"),
            ("\"a\u{7f}b\"", "a\u{7f}b"),
            (r#""\u0000""#, "\u{0}"),
        ] {
            assert_eq!(
                Sent::parse(body.as_bytes()),
                Ok(Sent::String(text.to_string())),
                "{body}"
            );
        }
    }

    #[test]
    fn a_strict_reader_is_given_what_github_read() {
        for (body, read) in [
            (r#"{"b": 1, /* c */ "a": [1.50, "x""#, r#"["#),
            (r#"{"b": 1, "a": 2"#, r#"{"b":1,"a":2}"#),
            ("null", "null"),
            ("tru", "null"),
            ("", ""),
            ("  ", "  "),
            ("nonsense", "nonsense"),
            (
                r#"{"a": "\ud800", "a": 12345678901234567890123}"#,
                r#"{"a":12345678901234567890123}"#,
            ),
        ] {
            let given = as_github_reads(body.as_bytes());
            let given = std::str::from_utf8(&given).unwrap();
            match read {
                "[" => assert_eq!(given, r#"[1.5,"x"]"#, "{body}"),
                _ => assert_eq!(given, read, "{body}"),
            }
        }
    }

    #[test]
    fn a_body_nests_as_deep_as_a_strict_reader_takes_one() {
        for (open, close) in [("[", "]"), (r#"{"a":"#, "}")] {
            let nested = |depth: usize| format!("{}1{}", open.repeat(depth), close.repeat(depth));
            let deepest = nested(MAX_DEPTH);
            assert!(Sent::parse(deepest.as_bytes()).is_ok(), "{open}");
            assert!(
                serde_json::from_slice::<serde_json::Value>(&as_github_reads(deepest.as_bytes()))
                    .is_ok(),
                "{open}"
            );
            assert_eq!(
                Sent::parse(nested(MAX_DEPTH + 1).as_bytes()),
                Err(NotJson),
                "{open}"
            );
        }
    }
}
