//! Furigana: readings set small above the words they belong to, as Claude writes them with
//! HTML's ruby (`<ruby>漢字<rt>かんじ</rt></ruby>`). The markdown parser drops HTML tags but
//! keeps text, so each reading travels through it as Unicode's interlinear annotation
//! characters, and `Selectable` lays out the words alone and draws the readings over them.

use std::borrow::Cow;
use std::ops::Range;

use cosmic::iced::advanced::text::Span;
use cosmic::widget::markdown;

const ANCHOR: char = '\u{FFF9}';
const SEPARATOR: char = '\u{FFFA}';
const TERMINATOR: char = '\u{FFFB}';

/// A reading over the bytes `base` of a block's text.
#[derive(Debug, Clone, PartialEq)]
pub struct Ruby {
    pub base: Range<usize>,
    pub reading: String,
}

/// `markdown` with each `<ruby>` element written in annotation characters. One left open
/// stays as it is.
pub fn annotate(markdown: &str) -> Cow<'_, str> {
    if !markdown.contains("<ruby>") {
        return Cow::Borrowed(markdown);
    }
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(start) = rest.find("<ruby>") {
        let Some(len) = rest[start..].find("</ruby>") else {
            break;
        };
        out.push_str(&rest[..start]);
        annotate_element(&rest[start + "<ruby>".len()..start + len], &mut out);
        rest = &rest[start + len + "</ruby>".len()..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// One element's words and readings: `漢<rt>かん</rt>字<rt>じ</rt>` is two of each. The
/// parentheses `<rp>` adds for browsers without ruby are dropped.
fn annotate_element(inner: &str, out: &mut String) {
    let mut inner = inner.replace("<rb>", "").replace("</rb>", "");
    while let Some(start) = inner.find("<rp>") {
        let end = inner[start..]
            .find("</rp>")
            .map_or(inner.len(), |end| start + end + "</rp>".len());
        inner.replace_range(start..end, "");
    }
    let mut rest = inner.as_str();
    while let Some(open) = rest.find("<rt>") {
        let after = &rest[open + "<rt>".len()..];
        let (reading, next) = after.split_once("</rt>").unwrap_or((after, ""));
        out.push(ANCHOR);
        out.push_str(&rest[..open]);
        out.push(SEPARATOR);
        out.push_str(reading);
        out.push(TERMINATOR);
        rest = next;
    }
    out.push_str(rest);
}

/// How far into `text` a reply streaming in can be parsed from `from` up to `end` without
/// cutting a `<ruby>` element, or its opening tag, in two.
pub fn whole_up_to(text: &str, from: usize, end: usize) -> usize {
    let chunk = &text[from..end];
    if let Some(open) = chunk.rfind("<ruby>")
        && !chunk[open..].contains("</ruby>")
    {
        return from + open;
    }
    match chunk.rfind('<') {
        Some(open) if "<ruby>".starts_with(&chunk[open..]) => from + open,
        _ => end,
    }
}

/// `spans` without their readings, which come back as rubies over the text left.
pub fn split(spans: &[Span<'_, markdown::Uri>]) -> (Vec<Span<'static, markdown::Uri>>, Vec<Ruby>) {
    let (mut shown, mut rubies) = (Vec::with_capacity(spans.len()), Vec::new());
    let (mut len, mut base, mut reading) = (0, 0, None::<String>);
    for span in spans {
        let mut text = String::with_capacity(span.text.len());
        for c in span.text.chars() {
            match (c, &mut reading) {
                (ANCHOR, _) => base = len,
                (SEPARATOR, _) => reading = Some(String::new()),
                (TERMINATOR, _) => rubies.push(Ruby {
                    base: base..len,
                    reading: reading.take().unwrap_or_default(),
                }),
                (c, Some(reading)) => reading.push(c),
                (c, None) => {
                    text.push(c);
                    len += c.len_utf8();
                }
            }
        }
        let mut span = span.clone().to_static();
        span.text = Cow::Owned(text);
        shown.push(span);
    }
    (shown, rubies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruby_becomes_annotation_characters() {
        assert_eq!(
            annotate("<ruby>漢字<rt>かんじ</rt></ruby>を読む"),
            "\u{FFF9}漢字\u{FFFA}かんじ\u{FFFB}を読む"
        );
        assert_eq!(
            annotate("<ruby>漢<rp>(</rp><rt>かん</rt><rp>)</rp>字<rt>じ</rt></ruby>"),
            "\u{FFF9}漢\u{FFFA}かん\u{FFFB}\u{FFF9}字\u{FFFA}じ\u{FFFB}"
        );
        assert_eq!(
            annotate("<ruby>漢字<rt>か"),
            "<ruby>漢字<rt>か",
            "not closed yet"
        );
    }

    #[test]
    fn streaming_stops_before_an_unfinished_ruby() {
        let text = "今日は<ruby>漢字<rt>かん";
        assert_eq!(whole_up_to(text, 0, text.len()), "今日は".len());
        let text = "今日は<ru";
        assert_eq!(whole_up_to(text, 0, text.len()), "今日は".len());
        let text = "a < b";
        assert_eq!(whole_up_to(text, 0, text.len()), text.len());
    }

    #[test]
    fn readings_leave_the_text_and_point_at_their_words() {
        let annotated = annotate("今日は<ruby>漢字<rt>かんじ</rt></ruby>です");
        let (shown, rubies) = split(&[Span::new(annotated.as_ref())]);
        assert_eq!(shown[0].text, "今日は漢字です");
        assert_eq!(
            rubies,
            [Ruby {
                base: "今日は".len().."今日は漢字".len(),
                reading: "かんじ".to_string(),
            }]
        );
    }
}
