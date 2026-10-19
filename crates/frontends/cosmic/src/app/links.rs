//! Web addresses written out in text, which markdown only makes links of inside `<>`.

use std::borrow::Cow;
use std::ops::Range;

use cosmic::iced::Color;
use cosmic::iced::advanced::text::Span;
use cosmic::widget::markdown::Uri;

/// `spans` with every address in one that is not a link yet split out as a link.
pub fn linked(spans: Vec<Span<'static, Uri>>, color: Color) -> Vec<Span<'static, Uri>> {
    if !spans
        .iter()
        .any(|span| span.link.is_none() && span.text.contains("://"))
    {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let found: Vec<Range<usize>> = match span.link {
            None => addresses(&span.text).collect(),
            Some(_) => Vec::new(),
        };
        if found.is_empty() {
            out.push(span);
            continue;
        }
        let piece = |range: Range<usize>| Span {
            text: Cow::Owned(span.text[range].to_string()),
            ..span.clone()
        };
        let mut at = 0;
        for address in found {
            if address.start > at {
                out.push(piece(at..address.start));
            }
            let url = span.text[address.clone()].to_string();
            out.push(Span {
                color: Some(color),
                link: Some(url),
                ..piece(address.clone())
            });
            at = address.end;
        }
        if at < span.text.len() {
            out.push(piece(at..span.text.len()));
        }
    }
    out
}

/// Where `text` holds an `http` or `https` address, without the punctuation that ends a
/// sentence or closes a parenthesis around it.
fn addresses(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut from = 0;
    std::iter::from_fn(move || {
        while let Some(found) = text[from..].find("http") {
            let start = from + found;
            from = start + 4;
            let Some(separator) = ["s://", "://"]
                .into_iter()
                .find(|separator| text[from..].starts_with(separator))
            else {
                continue;
            };
            if text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            {
                continue;
            }
            let after = from + separator.len();
            let end = after
                + text[after..]
                    .find(|c: char| ends_address(c))
                    .unwrap_or(text.len() - after);
            let end = start + trimmed(&text[start..end]);
            from = end.max(from);
            if end > after {
                return Some(start..end);
            }
        }
        None
    })
}

/// Characters an address never holds: space, what quotes it, and CJK punctuation, which
/// Japanese puts right after one without a space.
fn ends_address(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, '<' | '>' | '"' | '`')
        || ('\u{3000}'..='\u{303F}').contains(&c)
        || ('\u{FF00}'..='\u{FFEF}').contains(&c)
}

/// How much of `address` is the address once the punctuation after it is dropped.
fn trimmed(address: &str) -> usize {
    let mut end = address.len();
    loop {
        let kept = &address[..end];
        let Some(last) = kept.chars().next_back() else {
            return end;
        };
        let unbalanced =
            |open, close| last == close && kept.matches(open).count() < kept.matches(close).count();
        if matches!(
            last,
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' | '_' | '~'
        ) || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<&str> {
        addresses(text).map(|range| &text[range]).collect()
    }

    #[test]
    fn addresses_end_where_their_sentence_takes_over() {
        assert_eq!(
            found("See https://github.com/tracel-ai/burn/pull/6111."),
            ["https://github.com/tracel-ai/burn/pull/6111"]
        );
        assert_eq!(
            found("(docs at http://docs.rs/iced) and https://a.b/c?d=1&e=(2), too"),
            ["http://docs.rs/iced", "https://a.b/c?d=1&e=(2)"]
        );
        assert_eq!(
            found("PRは https://github.com/a/b/pull/1。次に"),
            ["https://github.com/a/b/pull/1"]
        );
        assert_eq!(found("**https://x.io/a_b_**"), ["https://x.io/a_b"]);
        assert!(found("nothttps://x.io, https://, httpd://x and http").is_empty());
    }

    #[test]
    fn an_address_becomes_a_link_of_its_own_and_links_stay_as_they_are() {
        let red = Color::from_rgb(1.0, 0.0, 0.0);
        let spans = linked(
            vec![
                Span::new("Opened https://github.com/a/b/pull/7 for you"),
                Span::new("https://kept.io").link("https://elsewhere.io".to_string()),
            ],
            red,
        );
        let texts: Vec<(&str, Option<&str>)> = spans
            .iter()
            .map(|span| (span.text.as_ref(), span.link.as_deref()))
            .collect();
        assert_eq!(
            texts,
            [
                ("Opened ", None),
                (
                    "https://github.com/a/b/pull/7",
                    Some("https://github.com/a/b/pull/7")
                ),
                (" for you", None),
                ("https://kept.io", Some("https://elsewhere.io")),
            ]
        );
        assert_eq!(spans[1].color, Some(red));
    }
}
