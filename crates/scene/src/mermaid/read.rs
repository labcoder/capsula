//! The bounded D1 flowchart reader: a byte cursor over one source and the lexical forms the
//! grammar asks it for, each read forward once or refused with one content-free error code.
//!
//! A reduced port of Pele's flowchart lexer (MIT; the notice is in THIRD-PARTY-NOTICES.md).

use crate::{
    ErrorCode, Text,
    limits::MAX_ID_CHARS,
    record::{Head, NodeShape},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stroke {
    Normal,
    Dotted,
    Thick,
}

/// One link as the source wrote it.
pub(super) struct Link {
    pub label: Option<Text>,
    pub stroke: Stroke,
    pub head: Head,
}

/// Words Pele always reads as keywords, so they can't name a node.
const KEYWORDS: [&str; 10] = [
    "end",
    "subgraph",
    "style",
    "classDef",
    "class",
    "click",
    "linkStyle",
    "flowchart",
    "graph",
    "interpolate",
];

/// A byte cursor over one Mermaid source.
pub(super) struct Reader<'a> {
    pub src: &'a str,
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn peek(&self) -> Option<u8> {
        self.peek_at(0)
    }
    pub(super) fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.as_bytes().get(self.pos + offset).copied()
    }
    pub(super) fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }
    pub(super) fn spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }
    pub(super) fn at_end_of_statement(&self) -> bool {
        matches!(self.peek(), None | Some(b'\n' | b';'))
    }
    pub(super) fn skip_line(&mut self) {
        self.pos = self
            .rest()
            .find('\n')
            .map_or(self.src.len(), |end| self.pos + end);
    }
    /// Whether `word` starts here and doesn't continue as a longer word, as Pele's keywords.
    pub(super) fn keyword(&self, word: &str) -> bool {
        self.rest().starts_with(word) && !self.peek_at(word.len()).is_some_and(is_word)
    }
    /// A node's id up to its shape or link, refusing characters Pele would read into it and
    /// the words it reserves.
    pub(super) fn node_id(&mut self) -> Result<&'a str, ErrorCode> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            let link = b == b'-' && matches!(self.peek_at(1), Some(b'-' | b'.'));
            if link || !(is_word(b) || b == b'.' || b == b'-') {
                break;
            }
            self.pos += 1;
        }
        let id = &self.src[start..self.pos];
        // Pele keeps reading these into the id, which then falls outside `WireId`'s alphabet;
        // `@` also starts shape metadata, `:::` a class.
        if self
            .peek()
            .is_some_and(|b| b >= 0x80 || b"!#$%&'*+`?\\/:,@".contains(&b))
        {
            return Err(ErrorCode::Unsupported);
        }
        if id.is_empty() || KEYWORDS.contains(&id) {
            return Err(ErrorCode::Malformed);
        }
        source_id(id)
    }

    pub(super) fn shape(&mut self) -> Result<Option<(NodeShape, Text)>, ErrorCode> {
        let (shape, open, close): (_, _, &[u8]) =
            match (self.peek(), self.peek_at(1), self.peek_at(2)) {
                (Some(b'['), Some(b'('), _) => (NodeShape::Cylinder, 2, b")]"),
                // Subroutine, trapezoid, property, double-circle, ellipse, hexagon and flag shapes.
                (Some(b'['), Some(b'[' | b'/' | b'\\' | b'|'), _)
                | (Some(b'('), Some(b'('), Some(b'('))
                | (Some(b'('), Some(b'-'), _)
                | (Some(b'{'), Some(b'{'), _)
                | (Some(b'>'), ..) => return Err(ErrorCode::Unsupported),
                (Some(b'['), ..) => (NodeShape::Box, 1, b"]"),
                (Some(b'('), Some(b'('), _) => (NodeShape::Circle, 2, b"))"),
                (Some(b'('), Some(b'['), _) => (NodeShape::Pill, 2, b"])"),
                (Some(b'('), ..) => (NodeShape::Round, 1, b")"),
                (Some(b'{'), ..) => (NodeShape::Diamond, 1, b"}"),
                _ => return Ok(None),
            };
        self.pos += open;
        Ok(Some((shape, self.label(close)?)))
    }

    /// A shape, pipe or title label ending at `close`: one quoted string, or plain text
    /// without brackets, pipes, quotes or line breaks, as Pele's text state reads it.
    pub(super) fn label(&mut self, close: &[u8]) -> Result<Text, ErrorCode> {
        let content = if self.peek() == Some(b'"') {
            let content = self.quoted()?;
            self.spaces();
            content
        } else {
            let length = self
                .rest()
                .find(['[', ']', '(', ')', '{', '}', '|', '"', '\n'])
                .unwrap_or(self.rest().len());
            if length == 0 {
                return Err(ErrorCode::Malformed);
            }
            let content = &self.rest()[..length];
            self.pos += length;
            content
        };
        if !self.rest().as_bytes().starts_with(close) {
            return Err(ErrorCode::Malformed);
        }
        self.pos += close.len();
        text(content.trim())
    }

    /// A nonempty double-quoted string on one line; a backtick inside the quote is Markdown.
    pub(super) fn quoted(&mut self) -> Result<&'a str, ErrorCode> {
        if self.peek_at(1) == Some(b'`') {
            return Err(ErrorCode::Unsupported);
        }
        let rest = &self.rest()[1..];
        let length = rest.find(['"', '\n']).ok_or(ErrorCode::Malformed)?;
        if length == 0 || rest.as_bytes()[length] != b'"' {
            return Err(ErrorCode::Malformed);
        }
        self.pos += length + 2;
        Ok(&rest[..length])
    }

    /// One link, read forward once: a whole arrow with an optional `|label|`, or an arrow
    /// opening, its text and the closing arrow it ends at.
    pub(super) fn link(&mut self) -> Result<Link, ErrorCode> {
        match self.peek() {
            Some(b'~') if self.rest().starts_with("~~~") => return Err(ErrorCode::Unsupported),
            // Pele's cross and circle heads.
            Some(b'x' | b'o') if matches!(self.peek_at(1), Some(b'-' | b'=' | b'.')) => {
                return Err(ErrorCode::Unsupported);
            }
            Some(b'<' | b'-' | b'=' | b'.') => {}
            // `id@` before an arrow is Pele's edge id.
            Some(b) if is_word(b) => {
                let run = self
                    .rest()
                    .bytes()
                    .take_while(|&b| is_word(b) || b == b'.' || b == b'-')
                    .count();
                return Err(if self.peek_at(run) == Some(b'@') {
                    ErrorCode::Unsupported
                } else {
                    ErrorCode::Malformed
                });
            }
            _ => return Err(ErrorCode::Malformed),
        }
        let opening = if self.peek() == Some(b'<') {
            self.pos += 1;
            Head::Start
        } else {
            Head::None
        };
        let start = self.pos;
        while matches!(self.peek(), Some(b'-' | b'=' | b'.')) {
            self.pos += 1;
        }
        let body = &self.src[start..self.pos];
        let end = self.arrow_end()?;
        let (stroke, whole) = match (body, end) {
            ("--", Head::End) | ("---", Head::None) => (Stroke::Normal, true),
            ("-.-" | ".-", _) => (Stroke::Dotted, true),
            ("==", Head::End) | ("===", Head::None) => (Stroke::Thick, true),
            ("--", Head::None) => (Stroke::Normal, false),
            ("-.", Head::None) => (Stroke::Dotted, false),
            ("==", Head::None) => (Stroke::Thick, false),
            _ => return Err(longer(body)),
        };
        if !whole {
            return self.text_link(stroke, opening);
        }
        let head = heads(opening, end)?;
        self.spaces();
        let mut label = None;
        if self.peek() == Some(b'|') {
            self.pos += 1;
            label = Some(self.label(b"|")?).filter(|label| !label.as_str().is_empty());
        }
        Ok(Link {
            label,
            stroke,
            head,
        })
    }

    /// `End` when an arrow ends in `>`; Pele's cross and circle ends are refused.
    fn arrow_end(&mut self) -> Result<Head, ErrorCode> {
        match self.peek() {
            Some(b'>') => {
                self.pos += 1;
                Ok(Head::End)
            }
            Some(b'x' | b'o') => Err(ErrorCode::Unsupported),
            _ => Ok(Head::None),
        }
    }

    /// `A -- text --> B` and its dotted and thick forms: the text runs to the first closing
    /// mark for its stroke, which must then be a whole one-length arrow.
    fn text_link(&mut self, stroke: Stroke, opening: Head) -> Result<Link, ErrorCode> {
        self.spaces();
        let content = if self.peek() == Some(b'"') {
            let content = self.quoted()?;
            self.spaces();
            content
        } else {
            let bytes = self.rest().as_bytes();
            let mut length = 0;
            while let Some(&b) = bytes.get(length) {
                let closes = match stroke {
                    Stroke::Normal => b == b'-' && bytes.get(length + 1) == Some(&b'-'),
                    Stroke::Thick => b == b'=',
                    Stroke::Dotted => {
                        b == b'.' || (b == b'-' && bytes.get(length + 1) == Some(&b'.'))
                    }
                };
                if closes || b == b'"' || b == b'\n' {
                    break;
                }
                length += 1;
            }
            let content = &self.rest()[..length];
            match content.bytes().last() {
                // Pele reads a touching `<`, `x` or `o` as part of the closing arrow.
                None | Some(b'<') => return Err(ErrorCode::Malformed),
                Some(b'x' | b'o') => return Err(ErrorCode::Unsupported),
                Some(_) => {}
            }
            self.pos += length;
            content
        };
        let mark = match stroke {
            Stroke::Normal => b'-',
            Stroke::Dotted => {
                if self.peek() == Some(b'-') {
                    self.pos += 1;
                }
                b'.'
            }
            Stroke::Thick => b'=',
        };
        let start = self.pos;
        while self.peek() == Some(mark) {
            self.pos += 1;
        }
        let run = self.pos - start;
        if stroke == Stroke::Dotted {
            if self.peek() != Some(b'-') {
                return Err(ErrorCode::Malformed);
            }
            self.pos += 1;
        }
        let end = self.arrow_end()?;
        let expected = match (stroke, end) {
            (Stroke::Dotted, _) => 1,
            (_, Head::End) => 2,
            _ => 3,
        };
        if run != expected {
            return Err(if run > expected {
                ErrorCode::Unsupported
            } else {
                ErrorCode::Malformed
            });
        }
        Ok(Link {
            label: Some(text(content.trim())?).filter(|label| !label.as_str().is_empty()),
            stroke,
            head: heads(opening, end)?,
        })
    }
}

pub(super) fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A link's heads from its `<` opening and `>` end; one that opens with `<` must end with `>`.
fn heads(opening: Head, end: Head) -> Result<Head, ErrorCode> {
    match (opening, end) {
        (Head::Start, Head::End) => Ok(Head::Both),
        (Head::Start, _) => Err(ErrorCode::Malformed),
        _ => Ok(end),
    }
}

/// Pele's longer arrows only stretch its layout, which D1 has no use for; other runs of link
/// characters are malformed.
fn longer(body: &str) -> ErrorCode {
    let dots = body
        .strip_suffix('-')
        .map(|dots| dots.strip_prefix('-').unwrap_or(dots));
    if (body.len() >= 2 && (body.bytes().all(|b| b == b'-') || body.bytes().all(|b| b == b'=')))
        || dots.is_some_and(|dots| !dots.is_empty() && dots.bytes().all(|b| b == b'.'))
    {
        ErrorCode::Unsupported
    } else {
        ErrorCode::Malformed
    }
}

/// An id the source names, kept verbatim when it is in `WireId`'s alphabet.
pub(super) fn source_id(id: &str) -> Result<&str, ErrorCode> {
    if id.is_empty() {
        return Err(ErrorCode::Malformed);
    }
    if !id.bytes().all(|b| is_word(b) || b == b'.' || b == b'-') {
        return Err(ErrorCode::Unsupported);
    }
    if id.len() > MAX_ID_CHARS {
        return Err(ErrorCode::TooLong);
    }
    Ok(id)
}

/// Display text; markup and entity codes are refused rather than drawn literally.
pub(super) fn text(content: &str) -> Result<Text, ErrorCode> {
    if markup(content) || entity(content) {
        return Err(ErrorCode::Unsupported);
    }
    Text::try_from(content.to_owned())
}

/// A tag such as `<b>`, `</b>` or `<br/>`; a bare `<` as in `1 < 2` or `1<2` is text.
// shortcut: `<br>` stays refused, not a line break; accept it once scene text defines explicit
// breaks and slice 3's real runs show models writing it often.
fn markup(content: &str) -> bool {
    let bytes = content.as_bytes();
    let Some(close) = content.rfind('>') else {
        return false;
    };
    bytes[..close].iter().enumerate().any(|(open, &b)| {
        if b != b'<' {
            return false;
        }
        let mut name = open + 1;
        if bytes.get(name) == Some(&b'/') {
            name += 1;
        }
        if !bytes.get(name).is_some_and(u8::is_ascii_alphabetic) {
            return false;
        }
        while bytes.get(name).is_some_and(u8::is_ascii_alphanumeric) {
            name += 1;
        }
        bytes
            .get(name)
            .is_some_and(|&b| b == b'>' || b == b'/' || b.is_ascii_whitespace())
    })
}

/// An entity code such as `&amp;`, `&#35;` or Mermaid's own `#quot;`, which Pele decodes when
/// it draws; drawing the code itself would show different text.
fn entity(content: &str) -> bool {
    let bytes = content.as_bytes();
    bytes.iter().enumerate().any(|(start, &b)| {
        let mut code = start + 1;
        match b {
            b'&' if bytes.get(code) == Some(&b'#') => code += 1,
            b'&' | b'#' => {}
            _ => return false,
        }
        let first = code;
        while bytes.get(code).is_some_and(u8::is_ascii_alphanumeric) {
            code += 1;
        }
        code > first && bytes.get(code) == Some(&b';')
    })
}

#[cfg(test)]
mod tests {
    use super::{Reader, Stroke, source_id, text};
    use crate::{
        ErrorCode, Text,
        record::{Head, NodeShape},
    };

    fn at(src: &str) -> Reader<'_> {
        Reader { src, pos: 0 }
    }

    #[test]
    fn statement_boundaries_and_comment_lines_keep_the_cursor_exact() {
        let mut reader = at(" \t%% ignored\nstyle A;");
        reader.spaces();
        assert_eq!(reader.pos, 2);
        reader.skip_line();
        assert_eq!(reader.pos, 12);
        assert!(reader.at_end_of_statement());
        reader.pos = 13;
        assert!(reader.keyword("style"));
        assert!(!reader.keyword("sty"));
        reader.pos = 20;
        assert!(reader.at_end_of_statement());
        reader.pos = 21;
        assert!(reader.at_end_of_statement());
        assert_eq!(reader.peek_at(200), None);
    }

    #[test]
    fn ids_keep_the_wire_alphabet_and_its_length_limit() {
        let longest = "a".repeat(40);
        let over = "a".repeat(41);
        for (id, read) in [
            (longest.as_str(), Ok(longest.as_str())),
            ("--1.1", Ok("--1.1")),
            (over.as_str(), Err(ErrorCode::TooLong)),
            ("a b", Err(ErrorCode::Unsupported)),
            ("", Err(ErrorCode::Malformed)),
        ] {
            assert_eq!(source_id(id), read, "id {id:?}");
        }
        let mut reader = at("A1.b-c-->B");
        assert_eq!(reader.node_id(), Ok("A1.b-c"), "an id stops at a link");
        assert_eq!(reader.pos, 6, "the cursor stops at the link");
        for (src, code) in [
            ("end", ErrorCode::Malformed),
            ("A:::c", ErrorCode::Unsupported),
            ("A@{ shape: rect }", ErrorCode::Unsupported),
        ] {
            assert_eq!(at(src).node_id(), Err(code), "source {src:?}");
        }
    }

    #[test]
    fn a_trailing_hyphen_belongs_to_the_id_before_an_odd_shape() {
        let mut reader = at("odd->Vertex Text]");
        assert_eq!(reader.node_id(), Ok("odd-"));
        assert_eq!(reader.pos, 4);
        assert_eq!(reader.shape().err(), Some(ErrorCode::Unsupported));
        let mut bare = at("A-");
        assert_eq!(bare.node_id(), Ok("A-"));
        assert_eq!(bare.pos, 2);
        let mut before_link = at("A- --> B");
        assert_eq!(before_link.node_id(), Ok("A-"));
        assert_eq!(before_link.pos, 2);
    }

    #[test]
    fn text_keeps_its_length_limit_and_refuses_markup_and_entities() {
        let longest = "x".repeat(400);
        assert_eq!(
            text(&longest).as_ref().map(Text::as_str),
            Ok(longest.as_str()),
            "400 characters"
        );
        assert_eq!(
            text(&"x".repeat(401)).err(),
            Some(ErrorCode::TooLong),
            "401 characters"
        );
        for plain in ["1 < 2", "1<2", "a & b;", "#1"] {
            assert_eq!(
                text(plain).as_ref().map(Text::as_str),
                Ok(plain),
                "text {plain:?}"
            );
        }
        for refused in ["<b>x</b>", "a<br/>b", "a &amp; b", "&#35;", "#quot;"] {
            assert_eq!(
                text(refused).err(),
                Some(ErrorCode::Unsupported),
                "text {refused:?}"
            );
        }
    }

    #[test]
    fn labels_and_shapes_end_at_their_closing_mark() {
        let mut reader = at("\"a b\" ]x");
        let label = reader.label(b"]").expect("a quoted label");
        assert_eq!(label.as_str(), "a b");
        assert_eq!(reader.pos, 7, "the cursor passes the closing mark");
        for (src, code) in [
            ("\"\"]", ErrorCode::Malformed),
            ("\"`md`\"]", ErrorCode::Unsupported),
            ("a\nb]", ErrorCode::Malformed),
        ] {
            assert_eq!(at(src).label(b"]").err(), Some(code), "label {src:?}");
        }
        let (shape, label) = at("[(db)]").shape().expect("D1").expect("a shape");
        assert_eq!((shape, label.as_str()), (NodeShape::Cylinder, "db"));
        assert_eq!(at("{{h}}").shape().err(), Some(ErrorCode::Unsupported));
        assert!(matches!(at(" A").shape(), Ok(None)), "no shape");
    }

    #[test]
    fn links_read_their_stroke_heads_label_and_length() {
        for (src, stroke, head, label, end) in [
            ("-->|go| B", Stroke::Normal, Head::End, Some("go"), 7),
            ("-- go --> B", Stroke::Normal, Head::End, Some("go"), 9),
            ("-.-> B", Stroke::Dotted, Head::End, None, 5),
            ("<==> B", Stroke::Thick, Head::Both, None, 5),
        ] {
            let mut reader = at(src);
            let link = reader.link().expect("a D1 link");
            let shown = link.label.as_ref().map(Text::as_str);
            assert_eq!(
                (link.stroke, link.head, shown, reader.pos),
                (stroke, head, label, end),
                "link {src:?}"
            );
        }
        for (src, code) in [
            ("----> B", ErrorCode::Unsupported),
            ("--x B", ErrorCode::Unsupported),
            ("~~~ B", ErrorCode::Unsupported),
            ("<--- B", ErrorCode::Malformed),
            ("-- B", ErrorCode::Malformed),
        ] {
            assert_eq!(at(src).link().err(), Some(code), "link {src:?}");
        }
    }
}
