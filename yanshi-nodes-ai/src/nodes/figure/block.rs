//! `domain: block` (Y8.4): block diagrams (systems, pipelines, control
//! loops) in TikZ.
//!
//! With a `spec` (`{blocks: [{id, label}], links: [{from, to, label?}]}`),
//! the source is checked before it is rendered: every block must be a
//! TikZ node named by its id and showing its label, and every link a
//! `\draw` path on which `from` comes right before `to` (a path drawn
//! with `<-` counts backwards). A violation is fed back to the model like
//! a compile error.

use std::collections::HashMap;

use serde_json::Value;

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

/// The prompt addendum: conventions, a skeleton, and the diagram.
pub(super) fn instructions(diagram: &Diagram) -> String {
  let mut text = String::from(
    "Draw a block diagram with TikZ. Conventions:\n\
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
     right=of in] (out) {Output};\n\\draw[-{Stealth}] (in) -- (out);\n\\end{tikzpicture}\n",
  );
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

/// Each `\draw` / `\path`: the node names it passes through, in arrow
/// order (reversed for `<-`), and whether it points both ways.
fn paths(source: &str, nodes: &HashMap<String, String>) -> Vec<(Vec<String>, bool)> {
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
    found.push((sequence, both));
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

/// Check `source` against `diagram`; the error lists every violation.
pub(super) fn check(source: &str, diagram: &Diagram) -> Result<(), String> {
  let mut problems = Vec::new();
  if !source.contains("\\begin{tikzpicture}") {
    problems.push("draw the diagram inside a tikzpicture environment".to_string());
  }
  let nodes = named_nodes(source);
  for block in &diagram.blocks {
    match nodes.get(&block.id) {
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
  let drawn = paths(source, &nodes);
  for link in &diagram.links {
    let linked = drawn.iter().any(|(sequence, both)| {
      sequence.windows(2).any(|pair| {
        (pair[0] == link.from && pair[1] == link.to)
          || (*both && pair[0] == link.to && pair[1] == link.from)
      })
    });
    if !linked {
      problems.push(format!(
        "the link {} -> {} is missing: draw it as \\draw[-{{Stealth}}] ({}) -- ({});",
        link.from, link.to, link.from, link.to
      ));
    }
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
    assert_eq!(check(LOOP, &control_loop()), Ok(()));
    let text = instructions(&control_loop());
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
    let error = check(source, &control_loop()).expect_err("violations");
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
        &diagram(json!({"blocks": [{"id": "a", "label": "A"}]}))
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
}
