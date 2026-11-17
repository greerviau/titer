//! A small XML event reader for the report formats titer reads: JUnit XML and Cobertura.
//! It handles elements, attributes, text, CDATA, comments, and the standard entities. It
//! does not validate, and it ignores DTDs and namespaces.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        /// `<x/>`: no End event follows.
        empty: bool,
    },
    End(String),
    Text(String),
}

/// Parses `xml` into events. Malformed markup ends the stream early.
pub fn events(xml: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest);
            break;
        };
        push_text(&mut out, &rest[..lt]);
        rest = &rest[lt..];
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let Some(end) = body.find("]]>") else { break };
            out.push(Event::Text(body[..end].to_string()));
            rest = &body[end + 3..];
        } else if rest.starts_with("<!--") {
            let Some(end) = rest.find("-->") else { break };
            rest = &rest[end + 3..];
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            let Some(end) = rest.find('>') else { break };
            rest = &rest[end + 1..];
        } else if let Some(body) = rest.strip_prefix("</") {
            let Some(end) = body.find('>') else { break };
            out.push(Event::End(body[..end].trim().to_string()));
            rest = &body[end + 1..];
        } else {
            let Some(end) = tag_end(rest) else { break };
            let inner = &rest[1..end];
            let (inner, empty) = match inner.strip_suffix('/') {
                Some(i) => (i, true),
                None => (inner, false),
            };
            let name_end = inner
                .find(|c: char| c.is_whitespace())
                .unwrap_or(inner.len());
            out.push(Event::Start {
                name: inner[..name_end].to_string(),
                attrs: parse_attrs(&inner[name_end..]),
                empty,
            });
            rest = &rest[end + 1..];
        }
    }
    out
}

fn push_text(out: &mut Vec<Event>, raw: &str) {
    if !raw.is_empty() {
        out.push(Event::Text(unescape(raw)));
    }
}

/// The index of the `>` that closes the tag at the start of `text`, skipping quoted values.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_attrs(text: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = text.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let Some(close) = after[1..].find(quote) else {
            break;
        };
        attrs.push((key, unescape(&after[1..1 + close])));
        rest = after[close + 2..].trim_start();
    }
    attrs
}

/// Replaces the predefined entities and numeric character references.
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|s| *s <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_elements_attributes_text_and_entities() {
        let xml = "<?xml version=\"1.0\"?><!-- c --><a x=\"1 &lt; 2\" y='q&quot;'><b/>t&amp;u&#10;<![CDATA[<raw>]]></a>";
        assert_eq!(
            events(xml),
            vec![
                Event::Start {
                    name: "a".into(),
                    attrs: vec![("x".into(), "1 < 2".into()), ("y".into(), "q\"".into())],
                    empty: false
                },
                Event::Start {
                    name: "b".into(),
                    attrs: vec![],
                    empty: true
                },
                Event::Text("t&u\n".into()),
                Event::Text("<raw>".into()),
                Event::End("a".into()),
            ]
        );
    }

    #[test]
    fn keeps_unknown_entities_and_bare_ampersands() {
        assert_eq!(unescape("a & b &bogus; &#x41;"), "a & b &bogus; A");
    }
}
