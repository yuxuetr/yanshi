//! `domain: block` (Y8.4): block diagrams (systems, pipelines, control
//! loops) in TikZ or D2.
//!
//! With a `spec` (`{blocks: [{id, label}], links: [{from, to, label?}]}`),
//! the source is checked before it is rendered. In TikZ every block must
//! be a node named by its id and showing its label, and every link a
//! `\draw` path on which `from` comes right before `to` (a path drawn
//! with `<-` counts backwards). In D2 every block must be a shape keyed by
//! its id with its label, and every link a connection `from -> to` (or
//! `to <- from`, or `<->`). A violation is fed back to the model like a
//! compile error.

use std::collections::HashMap;

use serde_json::Value;

use super::Language;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Block {
  id: String,
  label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
  from: String,
  to: String,
  label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Diagram {
  blocks: Vec<Block>,
  links: Vec<Link>,
}

const TIKZ_CONVENTIONS: &str = "Draw a block diagram with TikZ. Conventions:\n\
  - \\documentclass[border=4pt]{standalone}, \\usepackage{tikz}, \
  \\usetikzlibrary{positioning,arrows.meta}, one tikzpicture environment.\n\
  - Draw each block as a named node, `\\node[draw, rounded corners, minimum width=2.6cm, \
  minimum height=1cm] (<id>) {<label>};`, using the id as the node name and the label as \
  its text; place blocks with `right=of <id>` / `below=of <id>`.\n\
  - Draw each link as its own arrow from block to block, `\\draw[-{Stealth}] (<from>) -- \
  (<to>);` (use -| or |- for bends, and `node[midway, above]{<label>}` for a label).\n\
  Skeleton:\n\
  \\begin{tikzpicture}\n\\node[draw, rounded corners, minimum width=2.6cm, minimum height=1cm] \
  (in) {Input};\n\\node[draw, rounded corners, minimum width=2.6cm, minimum height=1cm, \
  right=of in] (out) {Output};\n\\draw[-{Stealth}] (in) -- (out);\n\\end{tikzpicture}\n";

const D2_CONVENTIONS: &str = "Draw a block diagram with D2. Conventions:\n\
  - Start with `direction: right`.\n\
  - Declare each block as `<id>: <label>`, using the id as the key and the label as its text \
  (quote a label that contains a colon).\n\
  - Draw each link as its own connection from block to block, `<from> -> <to>`, or \
  `<from> -> <to>: <label>` for a label.\n\
  Skeleton:\n\
  direction: right\nin: Input\nout: Output\nin -> out\n";

/// The prompt addendum: conventions, a skeleton, and the diagram.
pub(super) fn instructions(diagram: &Diagram, language: Language) -> String {
  let mut text = String::from(match language {
    Language::D2 => D2_CONVENTIONS,
    _ => TIKZ_CONVENTIONS,
  });
  if !diagram.blocks.is_empty() {
    text.push_str("Blocks (draw every one):\n");
    for block in &diagram.blocks {
      text.push_str(&format!("- ({}) {}\n", block.id, block.label));
    }
  }
  if !diagram.links.is_empty() {
    text.push_str("Links (draw every one, in this direction):\n");
    for link in &diagram.links {
      text.push_str(&format!(
        "- {} -> {}{}\n",
        link.from,
        link.to,
        link
          .label
          .as_deref()
          .map(|l| format!(", labelled \"{l}\""))
          .unwrap_or_default()
      ));
    }
  }
  text
}

fn is_identifier(id: &str) -> bool {
  id.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    && id
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse the `spec` input: `{blocks: [{id, label}], links: [{from, to, label?}]}`.
pub(super) fn parse_spec(spec: Option<&Value>) -> Result<Diagram, String> {
  let map = match spec {
    None | Some(Value::Null) => {
      return Ok(Diagram {
        blocks: Vec::new(),
        links: Vec::new(),
      });
    }
    Some(Value::Object(map)) => map,
    Some(_) => return Err("spec must be {blocks: [...], links: [...]}".to_string()),
  };
  let list = |key: &str| match map.get(key) {
    None | Some(Value::Null) => Ok(Vec::new()),
    Some(Value::Array(items)) => Ok(items.clone()),
    Some(_) => Err(format!("spec `{key}` must be an array")),
  };
  let text = |item: &Value, key: &str| {
    item
      .get(key)
      .and_then(Value::as_str)
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(str::to_string)
  };
  let blocks = list("blocks")?
    .iter()
    .enumerate()
    .map(|(i, item)| {
      let id = text(item, "id").ok_or_else(|| format!("block {i} needs an id"))?;
      if !is_identifier(&id) {
        return Err(format!(
          "block {i}: id '{id}' must be a letter followed by letters, digits, _ or -"
        ));
      }
      let label = text(item, "label").unwrap_or_else(|| id.clone());
      Ok(Block { id, label })
    })
    .collect::<Result<Vec<_>, String>>()?;
  let links = list("links")?
    .iter()
    .enumerate()
    .map(|(i, item)| {
      let end = |key: &str| {
        let id = text(item, key).ok_or_else(|| format!("link {i} needs `{key}`"))?;
        if blocks.iter().any(|b| b.id == id) {
          Ok(id)
        } else {
          Err(format!("link {i}: `{key}` names no block ('{id}')"))
        }
      };
      Ok(Link {
        from: end("from")?,
        to: end("to")?,
        label: text(item, "label"),
      })
    })
    .collect::<Result<Vec<_>, String>>()?;
  Ok(Diagram { blocks, links })
}

/// The balanced group that opens at `chars[at]` (`{`, `[` or `(`): its
/// inner text and the index just past it.
fn group(chars: &[char], at: usize) -> Option<(String, usize)> {
  let (open, close) = match chars.get(at)? {
    '{' => ('{', '}'),
    '[' => ('[', ']'),
    '(' => ('(', ')'),
    _ => return None,
  };
  let mut depth = 0;
  for (offset, &c) in chars[at..].iter().enumerate() {
    if c == open {
      depth += 1;
    } else if c == close {
      depth -= 1;
      if depth == 0 {
        return Some((chars[at + 1..at + offset].iter().collect(), at + offset + 1));
      }
    }
  }
  None
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
  while chars.get(i).is_some_and(|c| c.is_whitespace()) {
    i += 1;
  }
  i
}

fn word_at(chars: &[char], i: usize, word: &str) -> bool {
  let word: Vec<char> = word.chars().collect();
  chars.get(i..i + word.len()) == Some(&word[..])
    && !chars
      .get(i + word.len())
      .is_some_and(|c| c.is_ascii_alphabetic())
    && (i == 0 || !chars[i - 1].is_ascii_alphabetic())
}

/// Every named node: name → its text. Reads `node [opts] (name) [opts]
/// at (x,y) {text}` with the parts in any of the orders TikZ accepts.
fn named_nodes(source: &str) -> HashMap<String, String> {
  let chars: Vec<char> = source.chars().collect();
  let mut nodes = HashMap::new();
  let mut i = 0;
  while i < chars.len() {
    if !word_at(&chars, i, "node") {
      i += 1;
      continue;
    }
    let mut at = i + 4;
    let mut name = None;
    loop {
      at = skip_spaces(&chars, at);
      match chars.get(at) {
        Some('[') => match group(&chars, at) {
          Some((_, next)) => at = next,
          None => break,
        },
        Some('(') => match group(&chars, at) {
          Some((inner, next)) => {
            name = Some(inner.trim().to_string());
            at = next;
          }
          None => break,
        },
        Some('a') if word_at(&chars, at, "at") => {
          at = skip_spaces(&chars, at + 2);
          match group(&chars, at) {
            Some((_, next)) => at = next,
            None => break,
          }
        }
        Some('{') => {
          if let (Some(name), Some((text, _))) = (name.take(), group(&chars, at)) {
            nodes.insert(name, text);
          }
          break;
        }
        _ => break,
      }
    }
    i = at.max(i + 4);
  }
  nodes
}

/// A drawn link: from, to, and whether it points both ways.
type Edge = (String, String, bool);

/// The links of each `\draw` / `\path`: consecutive node names it passes
/// through, in arrow order (reversed for `<-`).
fn tikz_edges(source: &str, nodes: &HashMap<String, String>) -> Vec<Edge> {
  let chars: Vec<char> = source.chars().collect();
  let mut found = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    let keyword = ["\\draw", "\\path"]
      .into_iter()
      .find(|k| word_at(&chars, i + 1, &k[1..]) && chars[i] == '\\');
    let Some(keyword) = keyword else {
      i += 1;
      continue;
    };
    let mut at = skip_spaces(&chars, i + keyword.len());
    let mut options = String::new();
    if let Some((inner, next)) = group(&chars, at).filter(|_| chars.get(at) == Some(&'[')) {
      options = inner;
      at = next;
    }
    // The statement runs to the first `;` outside groups.
    let mut sequence = Vec::new();
    while at < chars.len() && chars[at] != ';' {
      match chars[at] {
        '{' | '[' => match group(&chars, at) {
          Some((_, next)) => at = next,
          None => break,
        },
        '(' => match group(&chars, at) {
          Some((inner, next)) => {
            // `(name)`, `(name.east)`, `([xshift=2pt]name.north)`.
            let inner = match inner.trim_start().strip_prefix('[') {
              Some(rest) => rest.split_once(']').map(|(_, r)| r).unwrap_or(rest),
              None => inner.as_str(),
            };
            let name = inner.trim().split('.').next().unwrap_or_default();
            if nodes.contains_key(name) {
              sequence.push(name.to_string());
            }
            at = next;
          }
          None => break,
        },
        _ => at += 1,
      }
    }
    let both = options.contains("<->");
    if options.contains("<-") && !both {
      sequence.reverse();
    }
    found.extend(
      sequence
        .windows(2)
        .map(|pair| (pair[0].clone(), pair[1].clone(), both)),
    );
    i = at;
  }
  found
}

/// Letters and digits only, lowercased, with TeX line breaks and command
/// names gone: `Plant\\ (motor)` ~ `plant motor`.
fn normalise(text: &str) -> String {
  let mut out = String::new();
  let mut chars = text.chars().peekable();
  while let Some(c) = chars.next() {
    if c == '\\' {
      while chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
        chars.next();
      }
    } else if c.is_alphanumeric() {
      out.extend(c.to_lowercase());
    }
  }
  out
}

/// Keys that set attributes rather than name shapes.
const D2_KEYWORDS: &[&str] = &[
  "direction",
  "shape",
  "style",
  "icon",
  "near",
  "tooltip",
  "link",
  "width",
  "height",
  "class",
  "classes",
  "vars",
  "label",
  "grid-rows",
  "grid-columns",
];

/// D2 statements with `#` comments dropped: the text between newlines,
/// `;`, `{` and `}` outside quotes.
fn d2_statements(source: &str) -> Vec<String> {
  let mut statements = vec![String::new()];
  let mut quote = None;
  let mut comment = false;
  for c in source.chars() {
    let current = statements.len() - 1;
    match (quote, comment, c) {
      (_, _, '\n') => {
        quote = None;
        comment = false;
        statements.push(String::new());
      }
      (_, true, _) => {}
      (Some(open), _, c) => {
        if c == open {
          quote = None;
        }
        statements[current].push(c);
      }
      (None, _, '"' | '\'') => {
        quote = Some(c);
        statements[current].push(c);
      }
      (None, _, '#') => comment = true,
      (None, _, ';' | '{' | '}') => statements.push(String::new()),
      (None, _, c) => statements[current].push(c),
    }
  }
  statements
    .into_iter()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect()
}

/// A statement's key and, after the first `:` outside quotes, its value.
fn d2_key_value(statement: &str) -> (&str, Option<&str>) {
  let mut quote = None;
  for (at, c) in statement.char_indices() {
    match (quote, c) {
      (Some(open), c) if c == open => quote = None,
      (Some(_), _) => {}
      (None, '"' | '\'') => quote = Some(c),
      (None, ':') => return (&statement[..at], Some(&statement[at + 1..])),
      _ => {}
    }
  }
  (statement, None)
}

/// A key split at its arrows (`->`, `<-`, `<->`, `--`, any number of
/// dashes): the endpoints and the arrows between them.
fn d2_arrows(key: &str) -> (Vec<String>, Vec<String>) {
  let chars: Vec<char> = key.chars().collect();
  let mut ends = vec![String::new()];
  let mut arrows = Vec::new();
  let mut quote = None;
  let mut i = 0;
  while i < chars.len() {
    let c = chars[i];
    if let Some(open) = quote {
      if c == open {
        quote = None;
      }
    } else if c == '"' || c == '\'' {
      quote = Some(c);
    } else if c == '<' || c == '-' {
      let mut j = i + usize::from(c == '<');
      let dashes = chars[j..].iter().take_while(|&&d| d == '-').count();
      j += dashes;
      let head = chars.get(j) == Some(&'>');
      j += usize::from(head);
      if dashes > 0 && (c == '<' || head || dashes > 1) {
        arrows.push(chars[i..j].iter().collect());
        ends.push(String::new());
        i = j;
        continue;
      }
    }
    if let Some(end) = ends.last_mut() {
      end.push(c);
    }
    i += 1;
  }
  (ends, arrows)
}

fn unquote(text: &str) -> String {
  let text = text.trim();
  ['"', '\'']
    .into_iter()
    .find_map(|q| text.strip_prefix(q).and_then(|t| t.strip_suffix(q)))
    .unwrap_or(text)
    .to_string()
}

/// The shapes (key → label; a shape named only in a connection is
/// labelled by its key, as D2 does) and the connections of a D2 source. A
/// shape inside a container (`group.a`) counts by its last key.
fn d2_diagram(source: &str) -> (HashMap<String, String>, Vec<Edge>) {
  let mut shapes = HashMap::new();
  let mut edges = Vec::new();
  for statement in d2_statements(source) {
    let (key, value) = d2_key_value(&statement);
    let (ends, arrows) = d2_arrows(key);
    if arrows.is_empty() {
      let mut segments: Vec<String> = key.split('.').map(unquote).collect();
      let labelled = segments.len() > 1 && segments.last().is_some_and(|s| s == "label");
      if labelled {
        segments.pop();
      }
      if segments.iter().any(|s| D2_KEYWORDS.contains(&s.as_str())) {
        continue;
      }
      let Some(name) = segments.pop().filter(|name| !name.is_empty()) else {
        continue;
      };
      match value.map(unquote).filter(|label| !label.is_empty()) {
        Some(label) => {
          shapes.insert(name, label);
        }
        None => {
          shapes.entry(name.clone()).or_insert(name);
        }
      }
      continue;
    }
    let names: Vec<String> = ends
      .iter()
      .map(|end| unquote(end.rsplit('.').next().unwrap_or_default()))
      .collect();
    for name in names.iter().filter(|name| !name.is_empty()) {
      shapes.entry(name.clone()).or_insert_with(|| name.clone());
    }
    for (pair, arrow) in names.windows(2).zip(&arrows) {
      let both = arrow.starts_with('<') && arrow.ends_with('>');
      let (from, to) = if arrow.starts_with('<') && !both {
        (&pair[1], &pair[0])
      } else {
        (&pair[0], &pair[1])
      };
      edges.push((from.clone(), to.clone(), both));
    }
  }
  (shapes, edges)
}

/// Check `source` (TikZ or D2) against `diagram`; the error lists every
/// violation.
pub(super) fn check(source: &str, diagram: &Diagram, language: Language) -> Result<(), String> {
  let mut problems = Vec::new();
  let d2 = language == Language::D2;
  let (nodes, edges) = if d2 {
    d2_diagram(source)
  } else {
    if !source.contains("\\begin{tikzpicture}") {
      problems.push("draw the diagram inside a tikzpicture environment".to_string());
    }
    let nodes = named_nodes(source);
    let edges = tikz_edges(source, &nodes);
    (nodes, edges)
  };
  for block in &diagram.blocks {
    match nodes.get(&block.id) {
      None if d2 => problems.push(format!(
        "block {} (\"{}\") is missing: declare it as `{}: {}`",
        block.id, block.label, block.id, block.label
      )),
      None => problems.push(format!(
        "block {} (\"{}\") is missing: draw it as \\node[draw] ({}) {{{}}};",
        block.id, block.label, block.id, block.label
      )),
      Some(text) if !normalise(text).contains(&normalise(&block.label)) => {
        problems.push(format!(
          "block {} shows \"{}\" but should show \"{}\"",
          block.id,
          text.trim(),
          block.label
        ));
      }
      Some(_) => {}
    }
  }
  for link in &diagram.links {
    let linked = edges.iter().any(|(from, to, both)| {
      (*from == link.from && *to == link.to) || (*both && *from == link.to && *to == link.from)
    });
    if linked {
      continue;
    }
    let hint = if d2 {
      format!("`{} -> {}`", link.from, link.to)
    } else {
      format!("\\draw[-{{Stealth}}] ({}) -- ({});", link.from, link.to)
    };
    problems.push(format!(
      "the link {} -> {} is missing: draw it as {hint}",
      link.from, link.to
    ));
  }
  if problems.is_empty() {
    Ok(())
  } else {
    Err(format!(
      "The diagram does not match its blocks and links:\n- {}",
      problems.join("\n- ")
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  const LOOP: &str = r"\begin{tikzpicture}
\node[draw, minimum width=2.6cm] (ctl) {Controller};
\node[draw, right=of ctl] (plant) {Plant\\(motor)};
\node (sensor) [draw, below=of plant] {Sensor};
\draw[-{Stealth}] (ctl) -- node[above]{u} (plant.west);
\draw[-{Stealth}] (plant) -- (sensor);
\draw[<-] (ctl.south) |- ([yshift=-2pt]sensor.west);
\end{tikzpicture}";

  fn diagram(value: Value) -> Diagram {
    parse_spec(Some(&value)).expect("spec")
  }

  fn control_loop() -> Diagram {
    diagram(json!({
      "blocks": [
        {"id": "ctl", "label": "Controller"},
        {"id": "plant", "label": "Plant (motor)"},
        {"id": "sensor", "label": "Sensor"}
      ],
      "links": [
        {"from": "ctl", "to": "plant", "label": "u"},
        {"from": "plant", "to": "sensor"},
        {"from": "sensor", "to": "ctl"}
      ]
    }))
  }

  #[test]
  fn a_diagram_with_every_block_and_link_passes() {
    assert_eq!(check(LOOP, &control_loop(), Language::Tikz), Ok(()));
    let text = instructions(&control_loop(), Language::Tikz);
    assert!(text.contains("- (plant) Plant (motor)"), "{text}");
    assert!(text.contains("- ctl -> plant, labelled \"u\""), "{text}");
  }

  #[test]
  fn missing_blocks_wrong_labels_and_missing_or_reversed_links_are_reported() {
    let source = r"\begin{tikzpicture}
\node[draw] (ctl) {Regulator};
\node[draw] (plant) {Plant (motor)};
\draw[->] (plant) -- (ctl);
\end{tikzpicture}";
    let error = check(source, &control_loop(), Language::Tikz).expect_err("violations");
    assert!(
      error.contains("block ctl shows \"Regulator\" but should show \"Controller\""),
      "{error}"
    );
    assert!(
      error.contains("block sensor (\"Sensor\") is missing"),
      "{error}"
    );
    assert!(
      error.contains("the link ctl -> plant is missing"),
      "reversed: {error}"
    );
    assert!(
      error.contains("the link sensor -> ctl is missing"),
      "{error}"
    );
    assert!(
      check(
        "\\node (a) {A};",
        &diagram(json!({"blocks": [{"id": "a", "label": "A"}]})),
        Language::Tikz
      )
      .expect_err("no environment")
      .contains("tikzpicture")
    );
  }

  #[test]
  fn bad_specs_are_rejected() {
    assert!(parse_spec(Some(&json!({"blocks": [{"label": "x"}]}))).is_err());
    assert!(parse_spec(Some(&json!({"blocks": [{"id": "1x"}]}))).is_err());
    assert!(
      parse_spec(Some(
        &json!({"blocks": [{"id": "a"}], "links": [{"from": "a", "to": "b"}]})
      ))
      .is_err()
    );
    assert!(parse_spec(Some(&json!([]))).is_err());
    assert_eq!(
      parse_spec(Some(&json!({"blocks": [{"id": "a"}]})))
        .expect("label defaults to id")
        .blocks[0]
        .label,
      "a"
    );
  }

  const D2_LOOP: &str = r#"# a control loop
direction: right
ctl: Controller { shape: rectangle }
plant.label: "Plant (motor)"
sensor
ctl -> plant: u
plant -> "sensor"; sensor <- nothing
ctl <- sensor # feedback
"#;

  #[test]
  fn a_d2_diagram_with_every_block_and_link_passes() {
    let sensor_labelled = diagram(json!({
      "blocks": [
        {"id": "ctl", "label": "Controller"},
        {"id": "plant", "label": "Plant (motor)"},
        {"id": "sensor"}
      ],
      "links": [
        {"from": "ctl", "to": "plant"},
        {"from": "plant", "to": "sensor"},
        {"from": "sensor", "to": "ctl"}
      ]
    }));
    assert_eq!(check(D2_LOOP, &sensor_labelled, Language::D2), Ok(()));
    let both_ways = "a: A\nb: B\na <-> b\nc: C\nb -> c -> a\n";
    let spec = diagram(json!({
      "blocks": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}, {"id": "c", "label": "C"}],
      "links": [{"from": "b", "to": "a"}, {"from": "c", "to": "a"}, {"from": "b", "to": "c"}]
    }));
    assert_eq!(check(both_ways, &spec, Language::D2), Ok(()));
    let text = instructions(&control_loop(), Language::D2);
    assert!(text.contains("D2"), "{text}");
    assert!(text.contains("- ctl -> plant, labelled \"u\""), "{text}");
    assert!(!text.contains("tikz"), "{text}");
  }

  #[test]
  fn d2_violations_are_reported_in_d2_terms() {
    let source = "ctl: Regulator\nplant: Plant (motor)\nplant -> ctl\n# sensor: Sensor\n";
    let error = check(source, &control_loop(), Language::D2).expect_err("violations");
    assert!(
      error.contains("block ctl shows \"Regulator\" but should show \"Controller\""),
      "{error}"
    );
    assert!(
      error.contains("block sensor (\"Sensor\") is missing: declare it as `sensor: Sensor`"),
      "commented out: {error}"
    );
    assert!(
      error.contains("the link ctl -> plant is missing: draw it as `ctl -> plant`"),
      "reversed: {error}"
    );
    assert!(!error.contains("tikzpicture"), "{error}");
  }
}
