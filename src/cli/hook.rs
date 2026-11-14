//! Claude Code hooks' input: a JSON object on stdin, of which the hooks need
//! only strings (`field`, `field_at`).

/// The string `key` holds at the top level of JSON object `json`, if it is
/// one.
pub fn field(json: &str, key: &str) -> Option<String> {
    field_at(json, &[key])
}

/// The string at `path` in JSON object `json`: `["tool_input", "file_path"]`
/// is `json.tool_input.file_path`.
pub fn field_at(json: &str, path: &[&str]) -> Option<String> {
    let mut p = Json {
        s: json.as_bytes(),
        i: 0,
    };
    for key in path {
        p.find(key)?;
    }
    p.string()
}

/// A cursor over JSON text; `peek`, `next` and `expect` skip whitespace first.
struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    /// In the object that starts here, moves to the value of `key`.
    fn find(&mut self, key: &str) -> Option<()> {
        self.expect(b'{')?;
        loop {
            let k = self.string()?; // `}`: none left
            self.expect(b':')?;
            if k == key {
                return Some(());
            }
            self.skip_value()?;
            self.expect(b',')?;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        (self.next()? == b).then_some(())
    }

    /// The next byte as is (in a string: whitespace counts).
    fn bump(&mut self) -> Option<u8> {
        let b = *self.s.get(self.i)?;
        self.i += 1;
        Some(b)
    }

    /// Four hex digits of a `\u` escape.
    fn hex4(&mut self) -> Option<u16> {
        let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u16::from_str_radix(hex, 16).ok()
    }

    /// A string, unescaped.
    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump()? {
                b'"' => return Some(out),
                b'\\' => {
                    let c = match self.bump()? {
                        b'u' => {
                            // A lone surrogate becomes U+FFFD.
                            let unit = self.hex4()?;
                            // A high surrogate pairs with a `\u` low one after it.
                            let low = match (0xD800..0xDC00).contains(&unit)
                                && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                            {
                                true => {
                                    self.i += 2;
                                    Some(self.hex4()?)
                                }
                                false => None,
                            };
                            let units = std::iter::once(unit).chain(low);
                            let chars = char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}'));
                            out.extend(chars);
                            continue;
                        }
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        e @ (b'"' | b'\\' | b'/') => e as char,
                        _ => return None,
                    };
                    out.push(c);
                }
                _ => {
                    // UTF-8 as is, up to the next quote or escape.
                    let start = self.i - 1;
                    let end = self.s[start..]
                        .iter()
                        .position(|c| matches!(c, b'"' | b'\\'))
                        .map_or(self.s.len(), |n| start + n);
                    out.push_str(std::str::from_utf8(&self.s[start..end]).ok()?);
                    self.i = end;
                }
            }
        }
    }

    /// Any value: a string, an object or array (strings in it skipped as
    /// strings), or a literal (number, `true`, `false`, `null`).
    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.string().map(drop),
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => self.string().map(drop)?,
                        b'{' | b'[' => {
                            depth += 1;
                            self.i += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            self.i += 1;
                            if depth == 0 {
                                return Some(());
                            }
                        }
                        _ => self.i += 1,
                    }
                }
            }
            _ => {
                let rest = &self.s[self.i..];
                self.i += rest
                    .iter()
                    .position(|c| matches!(c, b',' | b'}' | b']') || c.is_ascii_whitespace())
                    .unwrap_or(rest.len());
                Some(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Top-level strings, unescaped, past values of every kind.
    #[test]
    fn hook_input_fields() {
        let json = r#" { "session_id":"s", "n": -1.5e3, "nested": {"name": "inner", "a": [1, "]}", {}]},
            "ok": true, "name" : "bold-oak", "cwd": "/a \"b\"\\c/\u017c\u00f3\u0142w/zażółć/\ud83d\ude00" } "#;
        assert_eq!(field(json, "name").as_deref(), Some("bold-oak"));
        assert_eq!(field(json, "session_id").as_deref(), Some("s"));
        assert_eq!(
            field(json, "cwd").as_deref(),
            Some("/a \"b\"\\c/żółw/zażółć/😀")
        );
        assert_eq!(field(json, "missing"), None);
        assert_eq!(field(json, "n"), None, "not a string");
        assert_eq!(field("{}", "name"), None);
        assert_eq!(field("[\"name\"]", "name"), None);
        assert_eq!(field(r#"{"name": "unterminated"#, "name"), None);
        assert_eq!(
            field(r#"{"p": "\ud800\n"}"#, "p").as_deref(),
            Some("\u{FFFD}\n"),
            "a lone surrogate"
        );
        assert_eq!(
            field(r#"{"agent_id": "a1"}"#, "agent_id").as_deref(),
            Some("a1")
        );
        assert_eq!(
            field(r#"{"prompt": "\"agent_id\": x"}"#, "agent_id"),
            None,
            "only keys count"
        );
        let post =
            r#"{"tool_name":"Read","tool_input":{"limit":5,"file_path":"/m/ws/a.txt"},"cwd":"/m"}"#;
        assert_eq!(
            field_at(post, &["tool_input", "file_path"]).as_deref(),
            Some("/m/ws/a.txt")
        );
        assert_eq!(field_at(post, &["cwd"]).as_deref(), Some("/m"));
        assert_eq!(field_at(post, &["tool_input", "command"]), None);
        assert_eq!(field_at(post, &["tool_name", "x"]), None, "not an object");
    }
}
