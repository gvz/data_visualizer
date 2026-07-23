/// Parse one InfluxDB line-protocol line into `(topic, payload, ts)` tuples.
///
/// One line carries a measurement, optional sorted tags, one or more fields,
/// and an optional trailing nanosecond timestamp. Each field becomes its own
/// topic `measurement/<tagkey=tagval>/…/field`. A blank line, a comment
/// (`#…`), or a malformed line yields an empty vec (never panics). `now` is
/// used as the timestamp when the line omits one.
pub fn parse_line(line: &str, now: i64) -> Vec<(String, String, i64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Vec::new();
    }

    // Section split on unescaped, unquoted spaces: [measurement+tags, fields, ts?]
    let parts = split_unescaped(line, ' ');
    if parts.len() < 2 {
        return Vec::new();
    }

    let ts = if parts.len() >= 3 {
        match parts[2].parse::<i64>() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    } else {
        now
    };

    // measurement + tags
    let mut meta = split_unescaped(&parts[0], ',');
    if meta.is_empty() || meta[0].is_empty() {
        return Vec::new();
    }
    let measurement = unescape(&meta.remove(0));
    let mut tags: Vec<(String, String)> = Vec::new();
    for kv in &meta {
        let Some((k, v)) = split_key_value(kv) else {
            return Vec::new();
        };
        tags.push((unescape(&k), unescape(&v)));
    }
    tags.sort_by(|a, b| a.0.cmp(&b.0));

    let mut prefix = measurement;
    for (k, v) in &tags {
        prefix.push('/');
        prefix.push_str(k);
        prefix.push('=');
        prefix.push_str(v);
    }

    // fields
    let fields = split_unescaped(&parts[1], ',');
    let mut out = Vec::new();
    for kv in &fields {
        let Some((k, v)) = split_key_value(kv) else {
            continue;
        };
        let key = unescape(&k);
        if key.is_empty() {
            continue;
        }
        out.push((format!("{prefix}/{key}"), normalize_value(&v), ts));
    }
    out
}

/// Split `s` on unescaped, unquoted occurrences of `delim`. A backslash escapes
/// the next char (the pair is kept verbatim for later `unescape`); double quotes
/// protect the delimiter (kept verbatim for `normalize_value`).
fn split_unescaped(s: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    let mut in_quote = false;
    for c in s.chars() {
        if escaped {
            cur.push('\\');
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_quote = !in_quote;
            cur.push(c);
        } else if c == delim && !in_quote {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if escaped {
        cur.push('\\');
    }
    out.push(cur);
    out
}

/// Split on the first unescaped, unquoted `=` into (key, value).
fn split_key_value(s: &str) -> Option<(String, String)> {
    let mut escaped = false;
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => in_quote = !in_quote,
            '=' if !in_quote => return Some((s[..i].to_string(), s[i + 1..].to_string())),
            _ => {}
        }
    }
    None
}

/// Remove backslash escapes (`\x` -> `x`).
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// Strip Influx field-value type syntax so the payload parses by channel type.
fn normalize_value(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        return v[1..v.len() - 1].replace("\\\"", "\"");
    }
    match v {
        "t" | "T" | "true" | "True" | "TRUE" => return "true".to_string(),
        "f" | "F" | "false" | "False" | "FALSE" => return "false".to_string(),
        _ => {}
    }
    if let Some(stripped) = v.strip_suffix('i').or_else(|| v.strip_suffix('u')) {
        let digits = stripped.strip_prefix('-').unwrap_or(stripped);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return stripped.to_string();
        }
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_field_no_tags() {
        let out = parse_line("weather temperature=82 1000", 5);
        assert_eq!(out, vec![("weather/temperature".to_string(), "82".to_string(), 1000)]);
    }

    #[test]
    fn missing_timestamp_uses_now() {
        let out = parse_line("weather temperature=82", 5);
        assert_eq!(out, vec![("weather/temperature".to_string(), "82".to_string(), 5)]);
    }

    #[test]
    fn tags_included_and_sorted() {
        let out = parse_line("weather,zone=a,location=us temperature=82 1000", 0);
        assert_eq!(
            out,
            vec![(
                "weather/location=us/zone=a/temperature".to_string(),
                "82".to_string(),
                1000
            )]
        );
    }

    #[test]
    fn multiple_fields_fan_out_shared_ts() {
        let mut out = parse_line("weather temperature=82,humidity=71 1000", 0);
        out.sort();
        assert_eq!(
            out,
            vec![
                ("weather/humidity".to_string(), "71".to_string(), 1000),
                ("weather/temperature".to_string(), "82".to_string(), 1000),
            ]
        );
    }

    #[test]
    fn int_and_uint_suffix_stripped() {
        assert_eq!(parse_line("m a=82i 1", 0)[0].1, "82");
        assert_eq!(parse_line("m a=82u 1", 0)[0].1, "82");
        assert_eq!(parse_line("m a=-5i 1", 0)[0].1, "-5");
    }

    #[test]
    fn bool_normalized() {
        assert_eq!(parse_line("m a=t 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=T 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=true 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=f 1", 0)[0].1, "false");
        assert_eq!(parse_line("m a=FALSE 1", 0)[0].1, "false");
    }

    #[test]
    fn quoted_string_value_unquoted() {
        let out = parse_line(r#"m a="too hot" 1"#, 0);
        assert_eq!(out, vec![("m/a".to_string(), "too hot".to_string(), 1)]);
    }

    #[test]
    fn quoted_string_inner_escape() {
        let out = parse_line(r#"m a="say \"hi\"" 1"#, 0);
        assert_eq!(out[0].1, r#"say "hi""#);
    }

    #[test]
    fn escaped_space_in_measurement() {
        let out = parse_line(r"a\ b temperature=1 1", 0);
        assert_eq!(out, vec![("a b/temperature".to_string(), "1".to_string(), 1)]);
    }

    #[test]
    fn escaped_equals_in_tag_value() {
        let out = parse_line(r"m,k=a\=b v=1 1", 0);
        assert_eq!(out, vec![("m/k=a=b/v".to_string(), "1".to_string(), 1)]);
    }

    #[test]
    fn blank_and_comment_are_empty() {
        assert!(parse_line("", 0).is_empty());
        assert!(parse_line("   ", 0).is_empty());
        assert!(parse_line("# a comment", 0).is_empty());
    }

    #[test]
    fn malformed_no_fields_is_empty() {
        assert!(parse_line("justmeasurement", 0).is_empty());
    }

    #[test]
    fn non_integer_timestamp_rejects_line() {
        assert!(parse_line("m a=1 not_a_number", 0).is_empty());
    }
}
