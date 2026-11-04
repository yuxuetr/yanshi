//! `domain: circuit` (Y6.5): electrical schematics in circuitikz.
//!
//! A figure that compiles is not yet the figure that was asked for. With
//! a `spec` naming the components (`[{id, kind, value?}]`), the source
//! is checked before it is rendered: every component must be drawn as a
//! circuitikz bipole of its kind and labelled with its id. A violation
//! is fed back to the model like a compile error, so the retry loop
//! repairs content as well as syntax.

use serde_json::Value;

/// Component kinds and the circuitikz names that draw each.
const KINDS: &[(&str, &[&str])] = &[
  (
    "resistor",
    &[
      "R",
      "resistor",
      "american resistor",
      "european resistor",
      "vR",
      "variable resistor",
      "pR",
      "potentiometer",
      "generic",
    ],
  ),
  (
    "capacitor",
    &[
      "C",
      "capacitor",
      "eC",
      "ecapacitor",
      "polar capacitor",
      "pC",
      "cC",
      "curved capacitor",
      "vC",
      "variable capacitor",
    ],
  ),
  (
    "inductor",
    &[
      "L",
      "inductor",
      "cute inductor",
      "american inductor",
      "european inductor",
      "vL",
      "cuteinductor",
    ],
  ),
  (
    "voltage_source",
    &[
      "V",
      "vsource",
      "american voltage source",
      "european voltage source",
      "battery",
      "battery1",
      "battery2",
      "sV",
      "sinusoidal voltage source",
      "vsourcesin",
      "cV",
      "dcvsource",
      "sqV",
    ],
  ),
  (
    "current_source",
    &[
      "I",
      "isource",
      "american current source",
      "european current source",
      "cI",
      "sI",
      "dcisource",
      "isourcesin",
    ],
  ),
  (
    "diode",
    &[
      "D",
      "diode",
      "Do",
      "empty diode",
      "D*",
      "full diode",
      "zD",
      "Zener diode",
      "sD",
      "Schottky diode",
      "tD",
    ],
  ),
  (
    "led",
    &["leD", "led", "full led", "empty led", "leD*", "leDo"],
  ),
  (
    "switch",
    &[
      "switch",
      "spst",
      "nos",
      "closing switch",
      "ncs",
      "opening switch",
      "cspst",
      "ospst",
      "push button",
      "nopb",
      "ncpb",
      "toggle switch",
    ],
  ),
  ("lamp", &["lamp", "bulb"]),
  ("ammeter", &["ammeter", "rmeterwa"]),
  ("voltmeter", &["voltmeter", "rmeterwa"]),
];

/// The prompt addendum: conventions, a skeleton, and the parts list.
pub(super) fn instructions(spec: &[Component]) -> String {
  let mut text = String::from(
    "Draw an electrical schematic with circuitikz. Conventions:\n\
     - \\documentclass[border=4pt]{standalone}, \\usepackage{circuitikz}, one circuitikz environment.\n\
     - Draw each component as a bipole, `to[<kind>, l=$<id>$, a=$<value>$]`, with its id \
     as the label (R1 as $R_1$) and its value, if any, as the annotation.\n\
     - Place nodes on a grid (multiples of 1 or 2 cm), keep wires orthogonal, close every loop, \
     and draw ground as `node[ground]{}`.\n\
     Skeleton:\n\
     \\begin{circuitikz}\n\\draw (0,0) to[V, l=$V_1$, a=$5\\,V$] (0,3) to[R, l=$R_1$, a=$1\\,k\\Omega$] (3,3) \
     to[C, l=$C_1$] (3,0) -- (0,0);\n\\draw (0,0) node[ground]{};\n\\end{circuitikz}\n",
  );
  if !spec.is_empty() {
    text.push_str("Components (draw every one, labelled with its id):\n");
    for c in spec {
      text.push_str(&format!(
        "- {} ({}){}\n",
        c.id,
        c.kind,
        c.value
          .as_deref()
          .map(|v| format!(", {v}"))
          .unwrap_or_default()
      ));
    }
  }
  text
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Component {
  id: String,
  kind: String,
  value: Option<String>,
}

/// Parse the `spec` input: `[{id, kind, value?}]` or `{components: [...]}`.
pub(super) fn parse_spec(spec: Option<&Value>) -> Result<Vec<Component>, String> {
  let items = match spec {
    None | Some(Value::Null) => return Ok(Vec::new()),
    Some(Value::Array(items)) => items,
    Some(Value::Object(map)) => match map.get("components") {
      Some(Value::Array(items)) => items,
      _ => return Err("spec needs a `components` array".to_string()),
    },
    Some(_) => return Err("spec must be an array of components".to_string()),
  };
  items
    .iter()
    .enumerate()
    .map(|(i, item)| {
      let field = |key: &str| {
        item
          .get(key)
          .and_then(Value::as_str)
          .map(str::trim)
          .filter(|s| !s.is_empty())
      };
      let kind = field("kind").ok_or_else(|| format!("component {i} needs a kind"))?;
      if kind != "ground" && !KINDS.iter().any(|(k, _)| *k == kind) {
        let known: Vec<&str> = KINDS.iter().map(|(k, _)| *k).chain(["ground"]).collect();
        return Err(format!(
          "component {i}: unknown kind '{kind}' (expected one of {})",
          known.join(", ")
        ));
      }
      let id = match field("id") {
        Some(id) => id.to_string(),
        None if kind == "ground" => "ground".to_string(),
        None => return Err(format!("component {i} needs an id")),
      };
      Ok(Component {
        id,
        kind: kind.to_string(),
        value: field("value").map(str::to_string),
      })
    })
    .collect()
}

/// A bipole as drawn: its circuitikz name and the texts that may label it.
#[derive(Debug)]
struct Bipole {
  name: String,
  labels: Vec<String>,
}

/// Split `text` at top-level commas (outside `{}`, `[]` and `$…$`).
fn split_options(text: &str) -> Vec<String> {
  let mut parts = Vec::new();
  let mut depth = 0i32;
  let mut math = false;
  let mut current = String::new();
  for c in text.chars() {
    match c {
      '{' | '[' => depth += 1,
      '}' | ']' => depth -= 1,
      '$' => math = !math,
      ',' if depth == 0 && !math => {
        parts.push(std::mem::take(&mut current));
        continue;
      }
      _ => {}
    }
    current.push(c);
  }
  parts.push(current);
  parts
    .into_iter()
    .map(|p| p.trim().to_string())
    .filter(|p| !p.is_empty())
    .collect()
}

/// Every `to[...]` in the source, with nested brackets respected.
fn bipoles(source: &str) -> Vec<Bipole> {
  let mut found = Vec::new();
  let mut rest = source;
  while let Some(at) = rest.find("to[") {
    let body = &rest[at + 3..];
    let mut depth = 1i32;
    let mut end = None;
    for (i, c) in body.char_indices() {
      match c {
        '[' | '{' => depth += 1,
        ']' | '}' => {
          depth -= 1;
          if depth == 0 {
            end = Some(i);
            break;
          }
        }
        _ => {}
      }
    }
    let Some(end) = end else { break };
    let options = split_options(&body[..end]);
    if let Some((first, others)) = options.split_first() {
      let (name, own_label) = match first.split_once('=') {
        Some((name, label)) => (name.trim().to_string(), Some(label.trim().to_string())),
        None => (first.clone(), None),
      };
      let mut labels: Vec<String> = own_label.into_iter().collect();
      for option in others {
        if let Some((key, value)) = option.split_once('=')
          && matches!(
            key.trim(),
            "l" | "l_" | "l^" | "a" | "a_" | "a^" | "n" | "name"
          )
        {
          labels.push(value.trim().to_string());
        }
      }
      found.push(Bipole { name, labels });
    }
    rest = &body[end..];
  }
  found
}

/// `$R_{1}$` → `r1`: what is left of a label once TeX markup is gone.
fn normalise(label: &str) -> String {
  label
    .replace("\\mathrm", "")
    .replace("\\text", "")
    .chars()
    .filter(|c| !matches!(c, '$' | '{' | '}' | '_' | '^' | '\\' | ' '))
    .flat_map(char::to_lowercase)
    .collect()
}

/// Whether a label names `id`: it starts with the id and the id is not
/// the prefix of a longer one (`C1` must not match `C10`).
fn labels_id(label: &str, id: &str) -> bool {
  let (label, id) = (normalise(label), normalise(id));
  label
    .strip_prefix(&id)
    .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()))
}

/// Check `source` against `spec`; the error lists every violation, in a
/// form the model can act on.
pub(super) fn check(source: &str, spec: &[Component]) -> Result<(), String> {
  let mut problems = Vec::new();
  if !source.contains("\\begin{circuitikz}") {
    problems.push("draw the schematic inside a circuitikz environment".to_string());
  }
  let drawn = bipoles(source);
  for component in spec {
    if component.kind == "ground" {
      if !source.contains("ground]") {
        problems.push("ground is missing: add `node[ground]{}` at the ground node".to_string());
      }
      continue;
    }
    let names = KINDS
      .iter()
      .find(|(k, _)| *k == component.kind)
      .map(|(_, names)| *names)
      .unwrap_or_default();
    let labelled: Vec<&Bipole> = drawn
      .iter()
      .filter(|b| b.labels.iter().any(|l| labels_id(l, &component.id)))
      .collect();
    if labelled.is_empty() {
      problems.push(format!(
        "{} ({}) is missing: draw it as to[{}, l=${}$]",
        component.id,
        component.kind,
        names.first().copied().unwrap_or("R"),
        component.id
      ));
    } else if !labelled.iter().any(|b| names.contains(&b.name.as_str())) {
      problems.push(format!(
        "{} is drawn as `{}`, but it is a {}: use one of {}",
        component.id,
        labelled[0].name,
        component.kind,
        names.join(", ")
      ));
    }
  }
  if problems.is_empty() {
    Ok(())
  } else {
    Err(format!(
      "The schematic does not match the parts list:\n- {}",
      problems.join("\n- ")
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  const RC: &str = r"\begin{circuitikz}
\draw (0,0) to[V, l=$V_1$, a=$5\,V$] (0,3) to[R=$R_{1}$] (3,3) to[C, l_=$C_1$, a={$10\,\mu F$}] (3,0) -- (0,0);
\draw (0,0) node[ground]{};
\end{circuitikz}";

  fn spec(value: Value) -> Vec<Component> {
    parse_spec(Some(&value)).expect("spec")
  }

  #[test]
  fn a_schematic_with_every_part_passes() {
    let parts = spec(json!([
      {"id": "V1", "kind": "voltage_source", "value": "5 V"},
      {"id": "R1", "kind": "resistor"},
      {"id": "C1", "kind": "capacitor"},
      {"kind": "ground"}
    ]));
    assert_eq!(check(RC, &parts), Ok(()));
    assert!(instructions(&parts).contains("- V1 (voltage_source), 5 V"));
  }

  #[test]
  fn missing_mislabelled_and_wrong_kind_parts_are_reported() {
    let parts = spec(json!({"components": [
      {"id": "R1", "kind": "capacitor"},
      {"id": "L1", "kind": "inductor"},
      {"id": "C10", "kind": "capacitor"}
    ]}));
    let error = check(RC, &parts).expect_err("violations");
    assert!(
      error.contains("R1 is drawn as `R`, but it is a capacitor"),
      "{error}"
    );
    assert!(
      error.contains("L1 (inductor) is missing: draw it as to[L, l=$L1$]"),
      "{error}"
    );
    assert!(
      error.contains("C10 (capacitor) is missing"),
      "C1 is not C10: {error}"
    );
    assert!(
      check(
        "\\draw (0,0) to[R=$R_1$] (1,0);",
        &spec(json!([{"id": "R1", "kind": "resistor"}]))
      )
      .expect_err("no environment")
      .contains("circuitikz environment")
    );
    assert!(
      check(
        r"\begin{circuitikz}\end{circuitikz}",
        &spec(json!([{"kind": "ground"}]))
      )
      .expect_err("no ground")
      .contains("ground is missing")
    );
  }

  #[test]
  fn bad_specs_are_rejected() {
    assert!(parse_spec(Some(&json!([{"id": "X1", "kind": "flux capacitor"}]))).is_err());
    assert!(parse_spec(Some(&json!([{"kind": "resistor"}]))).is_err());
    assert!(parse_spec(Some(&json!("R1"))).is_err());
    assert_eq!(parse_spec(None), Ok(Vec::new()));
  }
}
