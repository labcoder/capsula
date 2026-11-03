//! The bounded D1 flowchart grammar: a whole source, or one content-free error code.
//!
//! A reduced port of Pele's flowchart parser (MIT; the notice is in THIRD-PARTY-NOTICES.md),
//! over the forms `Reader` scans. It keeps Pele's membership rule, where a node belongs to
//! the first group that closes naming it, and refuses recognised syntax outside scene v1's subset
//! as `Unsupported` instead of dropping it. Unknown header words and refused leading bytes are
//! also `Unsupported`; D1 does not distinguish another diagram kind from a misspelling.
//! Malformed or truncated D1 forms return `Malformed`.

use super::read::{Reader, Stroke, is_word, source_id, text};
use crate::{
    Budget, ErrorCode, Text,
    limits::MAX_OBJECT_DEPTH,
    record::{Direction, Head, NodeShape},
};
use std::collections::BTreeMap;

/// A flow direction. Scene v1's `dir` has no reversed values, so reversal stays private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Flow {
    pub dir: Direction,
    pub reverse: bool,
}

pub(super) struct Node<'a> {
    pub id: &'a str,
    pub label: Option<Text>,
    pub shape: NodeShape,
    /// The first group that closed naming this node; `None` is the flowchart's root.
    pub owner: Option<usize>,
    /// Where a shape first explicitly declared the node.
    scope: Option<Scope>,
}

/// The group a declaration sits in; `None` is the flowchart's root.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Scope(Option<usize>);

pub(super) struct Group<'a> {
    pub id: Option<&'a str>,
    pub title: Option<Text>,
    pub flow: Option<Flow>,
    pub parent: Option<usize>,
}

pub(super) struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Option<Text>,
    pub stroke: Stroke,
    pub head: Head,
}

/// Nodes in first-mention order, groups parent first, edges in source and fan-out order.
pub(super) struct Flowchart<'a> {
    pub flow: Flow,
    pub nodes: Vec<Node<'a>>,
    pub groups: Vec<Group<'a>>,
    pub edges: Vec<Edge>,
}
impl Flowchart<'_> {
    /// Objects this chart compiles to, its generated root included.
    pub(super) fn objects(&self) -> usize {
        1 + self.nodes.len() + self.groups.len() + self.edges.len()
    }
}

/// Parses a whole flowchart that compiles to at most `budget` objects, counting before it
/// stores anything.
/// # Errors
/// Returns the first failure; nothing from a failed source is kept.
pub(super) fn parse(src: &str, budget: usize) -> Result<Flowchart<'_>, ErrorCode> {
    let mut parser = Parser {
        read: Reader { src, pos: 0 },
        budget,
        chart: Flowchart {
            flow: Flow {
                dir: Direction::Down,
                reverse: false,
            },
            nodes: Vec::new(),
            groups: Vec::new(),
            edges: Vec::new(),
        },
        nodes: BTreeMap::new(),
        groups: BTreeMap::new(),
        open: Vec::new(),
    };
    parser.prelude()?;
    parser.chart.flow = parser.header()?;
    parser.take(1)?;
    parser.body()?;
    Ok(parser.chart)
}

struct Parser<'a> {
    read: Reader<'a>,
    budget: usize,
    chart: Flowchart<'a>,
    nodes: BTreeMap<&'a str, usize>,
    groups: BTreeMap<&'a str, usize>,
    /// Open groups, innermost last, each with the nodes its own body names.
    open: Vec<(usize, Vec<usize>)>,
}

impl<'a> Parser<'a> {
    fn take(&mut self, count: usize) -> Result<(), ErrorCode> {
        self.budget = self
            .budget
            .checked_sub(count)
            .ok_or(ErrorCode::OverBudget(Budget::LiveObjects))?;
        Ok(())
    }
    fn scope(&self) -> Option<usize> {
        self.open.last().map(|(group, _)| *group)
    }

    /// Skips blank and comment lines before the header, refusing configuration.
    fn prelude(&mut self) -> Result<(), ErrorCode> {
        loop {
            self.read.spaces();
            let rest = self.read.rest();
            if rest.is_empty() {
                return Err(ErrorCode::Malformed);
            }
            if rest.starts_with('\n') {
                self.read.pos += 1;
            } else if rest.starts_with("%%{") || rest.starts_with("---") {
                return Err(ErrorCode::Unsupported);
            } else if rest.starts_with("%%") {
                self.read.skip_line();
            } else {
                return Ok(());
            }
        }
    }

    fn header(&mut self) -> Result<Flow, ErrorCode> {
        let kind = self
            .read
            .rest()
            .bytes()
            .take_while(|&b| is_word(b) || b == b'-')
            .count();
        match &self.read.rest()[..kind] {
            "flowchart" | "graph" => self.read.pos += kind,
            "" => return Err(ErrorCode::Malformed),
            _ => return Err(ErrorCode::Unsupported),
        }
        self.read.spaces();
        let mut flow = Flow {
            dir: Direction::Down,
            reverse: false,
        };
        if !self.read.at_end_of_statement() {
            let word = self.read.rest().bytes().take_while(|&b| is_word(b)).count();
            let word = &self.read.rest()[..word];
            flow = match flow_of(word) {
                Some(flow) => flow,
                // Pele's symbolic aliases and `BR` are outside D1.
                None if matches!(word, "BR" | "v")
                    || (word.is_empty()
                        && matches!(self.read.peek(), Some(b'<' | b'>' | b'^'))) =>
                {
                    return Err(ErrorCode::Unsupported);
                }
                None => return Err(ErrorCode::Malformed),
            };
            self.read.pos += word.len();
            self.read.spaces();
        }
        if self.read.at_end_of_statement() {
            Ok(flow)
        } else {
            Err(ErrorCode::Malformed)
        }
    }

    fn body(&mut self) -> Result<(), ErrorCode> {
        let mut line_start = false;
        loop {
            self.read.spaces();
            match self.read.peek() {
                None => break,
                Some(b'\n') => {
                    self.read.pos += 1;
                    line_start = true;
                }
                Some(b';') => {
                    self.read.pos += 1;
                    line_start = false;
                }
                _ if line_start && self.read.rest().starts_with("%%{") => {
                    return Err(ErrorCode::Unsupported);
                }
                // Only whole-line comments, as Mermaid strips them before parsing.
                _ if line_start && self.read.rest().starts_with("%%") => self.read.skip_line(),
                _ => {
                    line_start = false;
                    self.statement()?;
                    self.read.spaces();
                    if !self.read.at_end_of_statement() {
                        return Err(ErrorCode::Malformed);
                    }
                }
            }
        }
        if self.open.is_empty() {
            Ok(())
        } else {
            Err(ErrorCode::Malformed)
        }
    }

    fn statement(&mut self) -> Result<(), ErrorCode> {
        if self.read.keyword("subgraph") {
            return self.open_group();
        }
        if self.read.keyword("end") {
            return self.close_group();
        }
        if self.read.keyword("direction")
            && let Some(flow) = self.direction()?
        {
            // Pele ignores a root `direction`; refusing it keeps the source and drawing in step.
            let group = self.scope().ok_or(ErrorCode::Unsupported)?;
            self.chart.groups[group].flow = Some(flow);
            return Ok(());
        }
        // Styles, classes, interactions and accessibility text are outside D1.
        if [
            "style",
            "classDef",
            "class",
            "click",
            "linkStyle",
            "accTitle",
            "accDescr",
        ]
        .iter()
        .any(|word| self.read.keyword(word))
            || (self.read.rest().starts_with("call")
                && self
                    .read
                    .peek_at(4)
                    .is_some_and(|b| b.is_ascii_whitespace()))
        {
            return Err(ErrorCode::Unsupported);
        }
        self.vertices()
    }

    /// A `direction` statement, or `None` when `direction` starts an ordinary node.
    fn direction(&mut self) -> Result<Option<Flow>, ErrorCode> {
        let rest = &self.read.rest()["direction".len()..];
        let word = rest.trim_start_matches([' ', '\t']);
        if word.len() == rest.len() {
            return Ok(None);
        }
        let mut length = word.bytes().take_while(|&b| is_word(b)).count();
        if length == 0 && word.starts_with(['<', '>', '^']) {
            length = 1;
        }
        let after = word[length..].trim_start_matches([' ', '\t', '\r']);
        if !(after.is_empty() || after.starts_with(['\n', ';'])) {
            return Ok(None);
        }
        let flow = match flow_of(&word[..length]) {
            Some(flow) => flow,
            None if matches!(&word[..length], "BR" | "v" | "<" | ">" | "^") => {
                return Err(ErrorCode::Unsupported);
            }
            None => return Ok(None),
        };
        self.read.pos = self.read.src.len() - after.len();
        Ok(Some(flow))
    }

    fn open_group(&mut self) -> Result<(), ErrorCode> {
        self.read.pos += "subgraph".len();
        if self.open.len() >= MAX_OBJECT_DEPTH {
            return Err(ErrorCode::TooDeep);
        }
        self.take(1)?;
        let (id, title) = self.group_header()?;
        if let Some(id) = id
            && (self.groups.contains_key(id) || self.nodes.contains_key(id))
        {
            // Reopening a group, or sharing an id with a node an edge could join, is outside D1.
            return Err(ErrorCode::Unsupported);
        }
        let index = self.chart.groups.len();
        self.chart.groups.push(Group {
            id,
            title,
            flow: None,
            parent: self.scope(),
        });
        if let Some(id) = id {
            self.groups.insert(id, index);
        }
        self.open.push((index, Vec::new()));
        Ok(())
    }

    /// `subgraph`, `subgraph id`, `subgraph id[title]` or a quoted header. As in Pele, an
    /// unbracketed header holding whitespace is an anonymous group's title, never an id.
    fn group_header(&mut self) -> Result<(Option<&'a str>, Option<Text>), ErrorCode> {
        let start = self.read.pos;
        self.read.spaces();
        if self.read.at_end_of_statement() {
            return Ok((None, None));
        }
        if self.read.pos == start {
            return Err(ErrorCode::Malformed);
        }
        let header = if self.read.peek() == Some(b'"') {
            let header = self.read.quoted()?;
            self.read.spaces();
            header
        } else {
            let length = self
                .read
                .rest()
                .find(['[', ';', '\n'])
                .unwrap_or(self.read.rest().len());
            let header = &self.read.rest()[..length];
            if header.contains([']', '(', ')', '{', '}', '|', '"']) {
                return Err(ErrorCode::Malformed);
            }
            self.read.pos += length;
            header
        }
        .trim();
        if self.read.peek() == Some(b'[') {
            self.read.pos += 1;
            let title = self.read.label(b"]")?;
            return Ok((
                Some(source_id(header)?),
                Some(title).filter(|title| !title.as_str().is_empty()),
            ));
        }
        if header.contains(char::is_whitespace) {
            return Ok((None, Some(text(header)?)));
        }
        Ok((Some(source_id(header)?), Some(text(header)?)))
    }

    fn close_group(&mut self) -> Result<(), ErrorCode> {
        self.read.pos += "end".len();
        let (group, named) = self.open.pop().ok_or(ErrorCode::Malformed)?;
        for node in named {
            let owner = &mut self.chart.nodes[node].owner;
            if owner.is_none() {
                *owner = Some(group);
            }
        }
        Ok(())
    }

    /// Nodes joined by links, each link from every node before it to every node after it.
    fn vertices(&mut self) -> Result<(), ErrorCode> {
        let mut from = self.fan()?;
        loop {
            self.read.spaces();
            if self.read.at_end_of_statement() {
                break;
            }
            let link = self.read.link()?;
            self.read.spaces();
            let to = self.fan()?;
            self.take(from.len() * to.len())?;
            for &start in &from {
                for &end in &to {
                    self.chart.edges.push(Edge {
                        from: start,
                        to: end,
                        label: link.label.clone(),
                        stroke: link.stroke,
                        head: link.head,
                    });
                }
            }
            from = to;
        }
        Ok(())
    }

    /// One node, or several joined by ` & `, which Pele requires spaced.
    fn fan(&mut self) -> Result<Vec<usize>, ErrorCode> {
        let mut nodes = vec![self.node()?];
        loop {
            let before = self.read.pos;
            self.read.spaces();
            if self.read.peek() != Some(b'&') {
                self.read.pos = before;
                return Ok(nodes);
            }
            self.read.pos += 1;
            let after = self.read.pos;
            self.read.spaces();
            if self.read.pos == after {
                return Err(ErrorCode::Malformed);
            }
            nodes.push(self.node()?);
        }
    }

    fn node(&mut self) -> Result<usize, ErrorCode> {
        let id = self.read.node_id()?;
        if self.groups.contains_key(id) {
            return Err(ErrorCode::Unsupported);
        }
        let shape = self.read.shape()?;
        if self.read.rest().starts_with(":::") {
            return Err(ErrorCode::Unsupported);
        }
        let index = if let Some(&index) = self.nodes.get(id) {
            index
        } else {
            self.take(1)?;
            self.chart.nodes.push(Node {
                id,
                label: None,
                shape: NodeShape::Box,
                owner: None,
                scope: None,
            });
            self.nodes.insert(id, self.chart.nodes.len() - 1);
            self.chart.nodes.len() - 1
        };
        if let Some((_, named)) = self.open.last_mut() {
            named.push(index);
        }
        if let Some((shape, label)) = shape {
            self.declare(index)?;
            let node = &mut self.chart.nodes[index];
            node.shape = shape;
            node.label = Some(label);
        }
        Ok(index)
    }

    /// Records a declaration; a later one may relabel a node, but only in the same group.
    fn declare(&mut self, node: usize) -> Result<(), ErrorCode> {
        let scope = Scope(self.scope());
        if *self.chart.nodes[node].scope.get_or_insert(scope) == scope {
            Ok(())
        } else {
            Err(ErrorCode::Unsupported)
        }
    }
}

fn flow_of(word: &str) -> Option<Flow> {
    let (dir, reverse) = match word {
        "TB" | "TD" => (Direction::Down, false),
        "BT" => (Direction::Down, true),
        "LR" => (Direction::Right, false),
        "RL" => (Direction::Right, true),
        _ => return None,
    };
    Some(Flow { dir, reverse })
}

#[cfg(test)]
mod tests {
    use super::{Stroke, parse};
    use crate::{Budget, ErrorCode, Text, record::Head};

    #[test]
    fn directions_titles_and_shapes_are_structural_facts() {
        let chart = parse(
            "flowchart RL\nsubgraph outer[Outer]\ndirection BT\nsubgraph inner[Inner]\nA((Seed))\nend\nend\n",
            4,
        ).expect("a root, two groups and a node");
        assert_eq!(
            (chart.flow.dir, chart.flow.reverse),
            (crate::record::Direction::Right, true)
        );
        let groups: Vec<_> = chart
            .groups
            .iter()
            .map(|group| {
                (
                    group.id,
                    group.title.as_ref().map(Text::as_str),
                    group.parent,
                    group.flow.map(|flow| (flow.dir, flow.reverse)),
                )
            })
            .collect();
        assert_eq!(
            groups,
            [
                (
                    Some("outer"),
                    Some("Outer"),
                    None,
                    Some((crate::record::Direction::Down, true))
                ),
                (Some("inner"), Some("Inner"), Some(0), None),
            ]
        );
        let node = &chart.nodes[0];
        assert_eq!(
            (
                node.shape,
                node.label.as_ref().map(Text::as_str),
                node.owner
            ),
            (crate::record::NodeShape::Circle, Some("Seed"), Some(1))
        );
    }

    #[test]
    fn the_budget_counts_every_compiled_object_before_storing_it() {
        let source = "flowchart LR\nA & B --> C & D\n";
        let chart = parse(source, 9).expect("the root, four nodes and four fan-out edges");
        assert_eq!(chart.objects(), 9, "every compiled object is counted");
        assert_eq!(
            parse(source, 8).err(),
            Some(ErrorCode::OverBudget(Budget::LiveObjects)),
            "one object over the budget fails"
        );
    }

    #[test]
    fn invalid_and_refused_sources_keep_their_codes() {
        let long_label = format!("flowchart LR\nA[{}]\n", "x".repeat(401));
        let long_id = format!("flowchart LR\n{}\n", "a".repeat(41));
        let deep = format!("flowchart LR\n{}", "subgraph\n".repeat(17));
        for (source, code) in [
            ("", ErrorCode::Malformed),
            ("flowchart LR\nA -->\n", ErrorCode::Malformed),
            ("flowchart LR\nsubgraph g\nA\n", ErrorCode::Malformed),
            ("flowchart BR\nA\n", ErrorCode::Unsupported),
            ("flowchat LR\nA\n", ErrorCode::Unsupported),
            ("flowchartLR\nA\n", ErrorCode::Unsupported),
            ("hello\n", ErrorCode::Unsupported),
            ("flowchart LR\n& A\n", ErrorCode::Unsupported),
            ("flowchart LR\nA;%% note\n", ErrorCode::Unsupported),
            (
                "flowchart TB\nsubgraph g\ndirection BR\nA\nend\n",
                ErrorCode::Unsupported,
            ),
            (
                "flowchart TB\nsubgraph g\ndirection <\nA\nend\n",
                ErrorCode::Unsupported,
            ),
            ("flowchart LR\nA ----> B\n", ErrorCode::Unsupported),
            ("flowchart LR\nA[\"a<br>b\"]\n", ErrorCode::Unsupported),
            (
                "flowchart LR\nA --> B[\"test\"]:::hot\n",
                ErrorCode::Unsupported,
            ),
            (
                "flowchart TB\nsubgraph one\nA[One]\nend\nsubgraph two\nA[Two]\nend\n",
                ErrorCode::Unsupported,
            ),
            ("flowchart LR\nA[a &amp; b]\n", ErrorCode::Unsupported),
            (
                "flowchart LR\nsubgraph g\nA\nend\nA --> g\n",
                ErrorCode::Unsupported,
            ),
            (long_label.as_str(), ErrorCode::TooLong),
            (long_id.as_str(), ErrorCode::TooLong),
            (deep.as_str(), ErrorCode::TooDeep),
        ] {
            assert_eq!(parse(source, 100).err(), Some(code), "source {source:?}");
        }
    }

    #[test]
    fn sixteen_closed_nested_groups_parse_and_seventeen_are_too_deep() {
        let nested = |depth: usize| {
            format!(
                "flowchart TB\n{}A\n{}",
                "subgraph\n".repeat(depth),
                "end\n".repeat(depth)
            )
        };
        let source = nested(16);
        let chart = parse(&source, 100).expect("sixteen nested groups");
        let parents: Vec<_> = chart.groups.iter().map(|group| group.parent).collect();
        let expected = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
            Some(7),
            Some(8),
            Some(9),
            Some(10),
            Some(11),
            Some(12),
            Some(13),
            Some(14),
        ];
        assert_eq!(parents, expected, "each group nests in the one before it");
        assert_eq!(
            chart.nodes[0].owner,
            Some(15),
            "the innermost group owns the node"
        );
        assert_eq!(parse(&nested(17), 100).err(), Some(ErrorCode::TooDeep));
    }

    #[test]
    fn the_first_group_to_close_owns_a_node_and_fanout_keeps_source_order() {
        let source = concat!(
            "flowchart TB\nsubgraph outer\nsubgraph inner\nA --> B\nend\n",
            "B --> C\nend\nC & A --> D\n"
        );
        let chart = parse(source, 100).expect("nested groups");
        let nodes: Vec<_> = chart
            .nodes
            .iter()
            .map(|node| (node.id, node.owner))
            .collect();
        assert_eq!(
            nodes,
            [("A", Some(1)), ("B", Some(1)), ("C", Some(0)), ("D", None)],
            "first mention order, owned by the first group to close"
        );
        let edges: Vec<_> = chart
            .edges
            .iter()
            .map(|edge| (edge.from, edge.to))
            .collect();
        assert_eq!(
            edges,
            [(0, 1), (1, 2), (2, 3), (0, 3)],
            "source and fan-out order"
        );
    }

    #[test]
    fn bare_membership_mentions_reference_prior_explicit_nodes() {
        let chart = parse(
            "graph TD\nA[Christmas] --> B(Go shopping)\nsubgraph 1test[old style that is broken]\nA\nend",
            5,
        ).expect("a bare group mention does not redefine the node");
        assert_eq!(
            (
                chart.nodes[0].id,
                chart.nodes[0].owner,
                chart.nodes[0].label.as_ref().map(Text::as_str)
            ),
            ("A", Some(0), Some("Christmas"))
        );
        assert_eq!(
            chart.groups[0].title.as_ref().map(Text::as_str),
            Some("old style that is broken")
        );
        let repeated = parse(
            "flowchart TB\nsubgraph first\nA\nend\nsubgraph second\nA\nend\n",
            4,
        )
        .expect("bare mentions keep first ownership");
        assert_eq!(repeated.nodes[0].owner, Some(0));
    }

    #[test]
    fn direction_can_still_name_a_node_in_a_link() {
        let chart = parse("flowchart TB\nsubgraph g\ndirection --> A\nend\n", 5)
            .expect("an ordinary node named direction");
        assert_eq!(
            chart.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
            ["direction", "A"]
        );
        assert_eq!(chart.groups[0].flow, None);
        assert_eq!((chart.edges[0].from, chart.edges[0].to), (0, 1));
    }

    #[test]
    fn links_keep_their_stroke_heads_and_label() {
        let chart = parse(
            "flowchart LR\nA -. maybe .-> B\nB == yes ==> C\nC <-->|both| D\nD --- E\n",
            100,
        )
        .expect("links");
        let links: Vec<_> = chart
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.stroke,
                    edge.head,
                    edge.label.as_ref().map(Text::as_str),
                )
            })
            .collect();
        assert_eq!(
            links,
            [
                (Stroke::Dotted, Head::End, Some("maybe")),
                (Stroke::Thick, Head::End, Some("yes")),
                (Stroke::Normal, Head::Both, Some("both")),
                (Stroke::Normal, Head::None, None),
            ],
            "each link keeps its own paint"
        );
    }
}
