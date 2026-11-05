//! `domain: chemistry` (Y8.4): structural formulas and reaction schemes
//! in chemfig.
//!
//! With a `spec` naming the molecules (`[{name, formula}]`, or
//! `{molecules: [...], reaction: true}`), the source is checked before it
//! is rendered: every molecule must be drawn as
//! `\chemname{\chemfig{…}}{name}`, and the atoms other than carbon and
//! hydrogen (which chemfig always writes out, while skeletal formulas
//! leave C and H implicit) must match its formula. Drawing ethanol where
//! acetic acid was asked for fails on the oxygen count. A violation is fed
//! back to the model like a compile error.

use std::collections::BTreeMap;

use serde_json::Value;

/// Element symbols, for telling `Cl` (chlorine) from `C` followed by a
/// stray letter.
const ELEMENTS: &[&str] = &[
  "H", "He", "Li", "Be", "B", "C", "N", "O", "F", "Ne", "Na", "Mg", "Al", "Si", "P", "S", "Cl",
  "Ar", "K", "Ca", "Sc", "Ti", "V", "Cr", "Mn", "Fe", "Co", "Ni", "Cu", "Zn", "Ga", "Ge", "As",
  "Se", "Br", "Kr", "Rb", "Sr", "Y", "Zr", "Nb", "Mo", "Tc", "Ru", "Rh", "Pd", "Ag", "Cd", "In",
  "Sn", "Sb", "Te", "I", "Xe", "Cs", "Ba", "La", "Ce", "Pr", "Nd", "Pm", "Sm", "Eu", "Gd", "Tb",
  "Dy", "Ho", "Er", "Tm", "Yb", "Lu", "Hf", "Ta", "W", "Re", "Os", "Ir", "Pt", "Au", "Hg", "Tl",
  "Pb", "Bi", "Po", "At", "Rn", "Fr", "Ra", "Ac", "Th", "Pa", "U", "Np", "Pu",
];

/// Element → count.
type Atoms = BTreeMap<String, u32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Molecule {
  name: String,
  formula: String,
  /// The formula's atoms other than C and H.
  heteroatoms: Atoms,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Scheme {
  molecules: Vec<Molecule>,
  reaction: bool,
}

/// The prompt addendum: conventions, a skeleton, and the molecules.
pub(super) fn instructions(scheme: &Scheme) -> String {
  let mut text = String::from(
    "Draw chemical structures with chemfig. Conventions:\n\
     - \\documentclass[border=4pt]{standalone}, \\usepackage{chemfig}.\n\
     - Draw each molecule as \\chemname{\\chemfig{<structure>}}{<name>}, with its name exactly \
     as given. Skeletal or condensed structures are both fine, but write every atom other than \
     carbon and hydrogen explicitly (O, N, S, Cl, ...).\n\
     - Use atom subscripts as H_2, branches as (...), rings as *6(...), and angles as [:30].\n",
  );
  if scheme.reaction {
    text.push_str(
      "- Draw the reaction as one scheme: \\schemestart <reactants with \\+ between them> \
       \\arrow{->} <products> \\schemestop.\n\
       Skeleton:\n\
       \\schemestart \\chemname{\\chemfig{H_3C-CH_2-OH}}{ethanol} \\arrow{->[\\chemfig{O_2}]} \
       \\chemname{\\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{acetic acid} \\schemestop\n",
    );
  } else {
    text.push_str("Skeleton:\n\\chemname{\\chemfig{H_3C-CH_2-OH}}{ethanol}\n");
  }
  if !scheme.molecules.is_empty() {
    text.push_str("Molecules (draw every one, named as given):\n");
    for molecule in &scheme.molecules {
      text.push_str(&format!("- {} ({})\n", molecule.name, molecule.formula));
    }
  }
  text
}

/// Parse the `spec` input: `[{name, formula}]` or
/// `{molecules: [...], reaction?: bool}`.
pub(super) fn parse_spec(spec: Option<&Value>) -> Result<Scheme, String> {
  let (items, reaction) = match spec {
    None | Some(Value::Null) => {
      return Ok(Scheme {
        molecules: Vec::new(),
        reaction: false,
      });
    }
    Some(Value::Array(items)) => (items, false),
    Some(Value::Object(map)) => match map.get("molecules") {
      Some(Value::Array(items)) => (
        items,
        map
          .get("reaction")
          .and_then(Value::as_bool)
          .unwrap_or(false),
      ),
      _ => return Err("spec needs a `molecules` array".to_string()),
    },
    Some(_) => return Err("spec must be an array of molecules".to_string()),
  };
  let molecules = items
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
      let name = field("name").ok_or_else(|| format!("molecule {i} needs a name"))?;
      let formula =
        field("formula").ok_or_else(|| format!("molecule {i} ({name}) needs a formula"))?;
      let mut heteroatoms =
        parse_formula(formula).map_err(|e| format!("molecule {i} ({name}): {e}"))?;
      heteroatoms.retain(|element, _| element != "C" && element != "H");
      Ok(Molecule {
        name: name.to_string(),
        formula: formula.to_string(),
        heteroatoms,
      })
    })
    .collect::<Result<Vec<_>, String>>()?;
  Ok(Scheme {
    molecules,
    reaction,
  })
}

/// The element symbol starting at `chars[i]`, if one does.
fn element_at(chars: &[char], i: usize) -> Option<&'static str> {
  let first = *chars.get(i)?;
  if !first.is_ascii_uppercase() {
    return None;
  }
  if let Some(&second) = chars.get(i + 1)
    && second.is_ascii_lowercase()
  {
    let pair: String = [first, second].iter().collect();
    if let Some(symbol) = ELEMENTS.iter().find(|e| **e == pair) {
      return Some(symbol);
    }
  }
  let single = first.to_string();
  ELEMENTS.iter().find(|e| **e == single).copied()
}

/// Digits starting at `chars[i]`: (value, length).
fn number_at(chars: &[char], i: usize) -> Option<(u32, usize)> {
  let digits: String = chars[i.min(chars.len())..]
    .iter()
    .take_while(|c| c.is_ascii_digit())
    .collect();
  let value = digits.parse().ok()?;
  Some((value, digits.len()))
}

/// `C2H4O2`, `Ca(OH)2`, `NH4+` → element counts.
fn parse_formula(formula: &str) -> Result<Atoms, String> {
  let chars: Vec<char> = formula.chars().filter(|c| !c.is_whitespace()).collect();
  let mut stack: Vec<Atoms> = vec![Atoms::new()];
  let mut i = 0;
  while i < chars.len() {
    let c = chars[i];
    if let Some(symbol) = element_at(&chars, i) {
      i += symbol.len();
      let (count, len) = number_at(&chars, i).unwrap_or((1, 0));
      i += len;
      if let Some(top) = stack.last_mut() {
        *top.entry(symbol.to_string()).or_insert(0) += count;
      }
    } else if c == '(' || c == '[' {
      stack.push(Atoms::new());
      i += 1;
    } else if c == ')' || c == ']' {
      i += 1;
      let (times, len) = number_at(&chars, i).unwrap_or((1, 0));
      i += len;
      let group = match (stack.pop(), stack.last_mut()) {
        (Some(group), Some(_)) => group,
        _ => return Err(format!("unbalanced ')' in formula '{formula}'")),
      };
      if let Some(top) = stack.last_mut() {
        for (element, count) in group {
          *top.entry(element).or_insert(0) += count * times;
        }
      }
    } else if matches!(c, '+' | '-' | '^')
      && chars[i..]
        .iter()
        .all(|c| matches!(c, '+' | '-' | '^') || c.is_ascii_digit())
    {
      // A trailing charge (`NH4+`, `SO4^2-`) says nothing about the atoms.
      break;
    } else {
      return Err(format!("cannot read '{c}' in formula '{formula}'"));
    }
  }
  match stack.pop() {
    Some(atoms) if stack.is_empty() => Ok(atoms),
    _ => Err(format!("unbalanced '(' in formula '{formula}'")),
  }
}

/// The text inside the balanced `{…}` (or `[…]`) that opens at `chars[at]`,
/// and the index just past it.
fn group(chars: &[char], at: usize) -> Option<(String, usize)> {
  let (open, close) = match chars.get(at)? {
    '{' => ('{', '}'),
    '[' => ('[', ']'),
    _ => return None,
  };
  let mut depth = 0;
  for (offset, &c) in chars[at..].iter().enumerate() {
    if c == open {
      depth += 1;
    } else if c == close {
      depth -= 1;
      if depth == 0 {
        let inner = chars[at + 1..at + offset].iter().collect();
        return Some((inner, at + offset + 1));
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

/// Every `\chemname{<drawing>}{<name>}`: (name, drawing).
fn named_drawings(source: &str) -> Vec<(String, String)> {
  let chars: Vec<char> = source.chars().collect();
  let needle: Vec<char> = "\\chemname".chars().collect();
  let mut found = Vec::new();
  let mut i = 0;
  while i + needle.len() <= chars.len() {
    if chars[i..i + needle.len()] != needle[..] {
      i += 1;
      continue;
    }
    let mut at = skip_spaces(&chars, i + needle.len());
    if chars.get(at) == Some(&'*') {
      at += 1;
    }
    at = skip_spaces(&chars, at);
    if chars.get(at) == Some(&'[')
      && let Some((_, next)) = group(&chars, at)
    {
      at = skip_spaces(&chars, next);
    }
    let Some((drawing, next)) = group(&chars, at) else {
      i += needle.len();
      continue;
    };
    let Some((name, next)) = group(&chars, skip_spaces(&chars, next)) else {
      i += needle.len();
      continue;
    };
    found.push((name, drawing));
    i = next;
  }
  found
}

/// The atoms other than C and H that a chemfig drawing writes out.
fn drawn_heteroatoms(drawing: &str) -> Atoms {
  // Bond and atom options (`[:30,,,,draw=red]`) and names (`@{a1}`) are
  // not atoms; neither are command names (`\charge`, `\lewis`), though
  // their arguments may hold atoms and stay.
  let chars: Vec<char> = drawing.chars().collect();
  let mut kept = Vec::with_capacity(chars.len());
  let mut i = 0;
  while i < chars.len() {
    match chars[i] {
      '[' => match group(&chars, i) {
        Some((_, next)) => i = next,
        None => i += 1,
      },
      '@' => {
        i += 1;
        if let Some((_, next)) = group(&chars, i) {
          i = next;
        }
      }
      '\\' => {
        i += 1;
        while chars.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
          i += 1;
        }
      }
      c => {
        kept.push(c);
        i += 1;
      }
    }
  }
  let mut atoms = Atoms::new();
  let mut i = 0;
  while i < kept.len() {
    let Some(symbol) = element_at(&kept, i) else {
      i += 1;
      continue;
    };
    i += symbol.len();
    // `H_2`, `H_{2}` or `H2`.
    let mut count = 1;
    let mut at = i;
    if kept.get(at) == Some(&'_') {
      at += 1;
      if kept.get(at) == Some(&'{') {
        at += 1;
      }
    }
    if let Some((value, len)) = number_at(&kept, at) {
      count = value;
      i = at + len;
    }
    if symbol != "C" && symbol != "H" {
      *atoms.entry(symbol.to_string()).or_insert(0) += count;
    }
  }
  atoms
}

/// Lowercase letters and digits only: `Acetic acid` ~ `acetic-acid`.
fn normalise(name: &str) -> String {
  name
    .chars()
    .filter(|c| c.is_alphanumeric())
    .flat_map(char::to_lowercase)
    .collect()
}

fn describe(atoms: &Atoms) -> String {
  if atoms.is_empty() {
    return "none".to_string();
  }
  atoms
    .iter()
    .map(|(element, count)| format!("{element}{count}"))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Check `source` against `scheme`; the error lists every violation.
pub(super) fn check(source: &str, scheme: &Scheme) -> Result<(), String> {
  let mut problems = Vec::new();
  if !source.contains("\\chemfig") {
    problems.push("draw the structures with \\chemfig".to_string());
  }
  if scheme.reaction && !source.contains("\\schemestart") {
    problems.push("draw the reaction as \\schemestart … \\arrow{->} … \\schemestop".to_string());
  }
  let drawings = named_drawings(source);
  for molecule in &scheme.molecules {
    let wanted = normalise(&molecule.name);
    let Some((_, drawing)) = drawings.iter().find(|(name, _)| normalise(name) == wanted) else {
      problems.push(format!(
        "{} is missing: draw it as \\chemname{{\\chemfig{{…}}}}{{{}}}",
        molecule.name, molecule.name
      ));
      continue;
    };
    let drawn = drawn_heteroatoms(drawing);
    if drawn != molecule.heteroatoms {
      problems.push(format!(
        "{} ({}) should have {} besides C and H, but the drawing has {}",
        molecule.name,
        molecule.formula,
        describe(&molecule.heteroatoms),
        describe(&drawn)
      ));
    }
  }
  if problems.is_empty() {
    Ok(())
  } else {
    Err(format!(
      "The structures do not match the molecule list:\n- {}",
      problems.join("\n- ")
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  const ESTERIFICATION: &str = r"\schemestart
\chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{acetic acid}
\+ \chemname{\chemfig{H_3C-CH_2-OH}}{Ethanol}
\arrow{->[H_2SO_4]}
\chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]O-CH_2-CH_3}}{ethyl acetate}
\+ \chemname{\chemfig{H_2O}}{water}
\schemestop";

  fn scheme(value: Value) -> Scheme {
    parse_spec(Some(&value)).expect("spec")
  }

  #[test]
  fn formulas_count_atoms_with_groups_and_charges() {
    let atoms = parse_formula("Ca(OH)2").expect("formula");
    assert_eq!(atoms["Ca"], 1);
    assert_eq!(atoms["O"], 2);
    assert_eq!(atoms["H"], 2);
    assert_eq!(parse_formula("NH4+").expect("ion")["H"], 4);
    assert_eq!(parse_formula("SO4^2-").expect("ion")["O"], 4);
    assert_eq!(parse_formula("C2H5Cl").expect("chloro")["Cl"], 1);
    assert!(parse_formula("C2H6O)").is_err());
    assert!(parse_formula("C2H6O?").is_err());
  }

  #[test]
  fn drawings_count_explicit_atoms_and_skip_options() {
    let atoms = drawn_heteroatoms(r"*6(-=-(-NO_2)=-(-Cl)=-[,,,,draw=red])");
    assert_eq!(atoms["N"], 1);
    assert_eq!(atoms["O"], 2);
    assert_eq!(atoms["Cl"], 1);
    assert_eq!(atoms.len(), 3, "{atoms:?}");
    assert!(drawn_heteroatoms(r"H_3C-CH_2-CH_3").is_empty());
    assert_eq!(drawn_heteroatoms(r"\charge{90=\:}{O}H_2")["O"], 1);
  }

  #[test]
  fn a_reaction_with_every_molecule_passes() {
    let parts = scheme(json!({"reaction": true, "molecules": [
      {"name": "acetic acid", "formula": "C2H4O2"},
      {"name": "ethanol", "formula": "C2H6O"},
      {"name": "ethyl acetate", "formula": "C4H8O2"},
      {"name": "water", "formula": "H2O"}
    ]}));
    assert_eq!(check(ESTERIFICATION, &parts), Ok(()));
    assert!(instructions(&parts).contains("- ethyl acetate (C4H8O2)"));
    assert!(instructions(&parts).contains("\\schemestart"));
  }

  #[test]
  fn missing_and_wrong_molecules_are_reported() {
    let parts = scheme(json!({"reaction": true, "molecules": [
      {"name": "ethanol", "formula": "C2H6O"},
      {"name": "water", "formula": "H2O"},
      {"name": "ammonia", "formula": "NH3"}
    ]}));
    let drawn =
      r"\chemname{\chemfig{H_3C-C(=[:60]O)-[:-60]OH}}{ethanol} \chemname{\chemfig{H_2O}}{water}";
    let error = check(drawn, &parts).expect_err("violations");
    assert!(error.contains("\\schemestart"), "{error}");
    assert!(
      error.contains("ethanol (C2H6O) should have O1 besides C and H, but the drawing has O2"),
      "{error}"
    );
    assert!(error.contains("ammonia is missing"), "{error}");
    assert!(!error.contains("water"), "{error}");
  }

  #[test]
  fn bad_specs_are_rejected() {
    assert!(parse_spec(Some(&json!([{"name": "x"}]))).is_err());
    assert!(parse_spec(Some(&json!([{"formula": "H2O"}]))).is_err());
    assert!(parse_spec(Some(&json!([{"name": "x", "formula": "H2O!"}]))).is_err());
    assert!(parse_spec(Some(&json!({"reaction": true}))).is_err());
    assert_eq!(
      parse_spec(None),
      Ok(Scheme {
        molecules: Vec::new(),
        reaction: false
      })
    );
  }
}
