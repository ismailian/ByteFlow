//! Minimal S3 XML parsing over quick-xml: entity-escaped keys, namespaces,
//! CDATA. Builds a tiny DOM (responses are ≤ 1000 keys) and reads leaves
//! verbatim — text is never trimmed, so keys with spaces round-trip.

use quick_xml::Reader;
use quick_xml::events::Event;

use bf_core::{AppFailure, AppResult};

#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

fn bad(e: impl std::fmt::Display) -> AppFailure {
    AppFailure::unknown(format!("malformed XML: {e}"))
}

fn local(name: &[u8]) -> String {
    let n = match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    };
    String::from_utf8_lossy(n).into_owned()
}

pub fn parse(xml: &str) -> AppResult<Node> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Node> = vec![Node::default()];
    loop {
        match reader.read_event().map_err(bad)? {
            Event::Start(e) => stack.push(Node { name: local(e.local_name().as_ref()), ..Default::default() }),
            Event::Empty(e) => {
                let n = Node { name: local(e.local_name().as_ref()), ..Default::default() };
                stack.last_mut().ok_or_else(|| bad("stack"))?.children.push(n);
            }
            Event::End(_) => {
                let n = stack.pop().ok_or_else(|| bad("unbalanced"))?;
                stack.last_mut().ok_or_else(|| bad("unbalanced"))?.children.push(n);
            }
            Event::Text(t) => {
                let s = t.decode().map_err(bad)?;
                stack.last_mut().ok_or_else(|| bad("stack"))?.text.push_str(&s);
            }
            Event::CData(t) => {
                let s = t.decode().map_err(bad)?;
                stack.last_mut().ok_or_else(|| bad("stack"))?.text.push_str(&s);
            }
            Event::GeneralRef(r) => {
                let resolved = if let Some(c) = r.resolve_char_ref().map_err(bad)? {
                    c.to_string()
                } else {
                    let name = r.decode().map_err(bad)?;
                    match name.as_ref() {
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "amp" => "&".into(),
                        "apos" => "'".into(),
                        "quot" => "\"".into(),
                        other => return Err(bad(format!("unknown entity {other}"))),
                    }
                };
                stack.last_mut().ok_or_else(|| bad("stack"))?.text.push_str(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack.pop().ok_or_else(|| bad("empty"))?;
    if !stack.is_empty() {
        return Err(bad("unterminated element"));
    }
    root.children.pop().ok_or_else(|| bad("no root element"))
}

/// `<Error><Code>…</Code><Message>…</Message></Error>` → (code, message).
pub fn parse_error(xml: &str) -> Option<(String, String)> {
    if !xml.contains("<Error") {
        return None;
    }
    let root = parse(xml).ok()?;
    let e = if root.name == "Error" { &root } else { root.child("Error")? };
    Some((e.text_of("Code")?.to_string(), e.text_of("Message").unwrap_or("").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listing_with_entities_and_ns() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><IsTruncated>true</IsTruncated>
  <Contents><Key>a &amp; b/ c&lt;d&gt;.txt</Key><Size>12</Size><ETag>&quot;abc&quot;</ETag></Contents>
  <Contents><Key><![CDATA[x/y z]]></Key><Size>0</Size></Contents>
  <Contents><Key>caf&#233; </Key><Size>1</Size></Contents>
  <NextContinuationToken>tok==</NextContinuationToken>
</ListBucketResult>"#;
        let r = parse(xml).unwrap();
        assert_eq!(r.name, "ListBucketResult");
        let keys: Vec<_> = r.children_named("Contents").map(|c| c.text_of("Key").unwrap().to_string()).collect();
        assert_eq!(keys, vec!["a & b/ c<d>.txt", "x/y z", "café "]);
        assert_eq!(r.children_named("Contents").next().unwrap().text_of("ETag"), Some("\"abc\""));
        assert_eq!(r.text_of("NextContinuationToken"), Some("tok=="));
    }

    #[test]
    fn error_body() {
        let xml = "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code><Message>nope</Message></Error>";
        assert_eq!(parse_error(xml), Some(("NoSuchKey".into(), "nope".into())));
        assert_eq!(parse_error("plain"), None);
    }
}
