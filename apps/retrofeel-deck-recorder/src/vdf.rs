use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::model::{ControllerBinding, ControllerLayoutMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Object(Vec<Entry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    String(String),
    Open,
    Close,
}

pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let tokens = tokenize(text)?;
    let mut cursor = 0;
    parse_entries(&tokens, &mut cursor, false)
}

pub fn controller_layout_map(
    text: &str,
    controller: u32,
    source_file: String,
) -> Result<ControllerLayoutMap> {
    let root = parse(text)?;
    let mappings = object_named(&root, "controller_mappings").unwrap_or(&root);
    let title = string_named(mappings, "title").unwrap_or_default();
    let description = string_named(mappings, "description").unwrap_or_default();
    let controller_type = string_named(mappings, "controller_type").unwrap_or_default();

    let mut active_groups = BTreeMap::<String, String>::new();
    if let Some(preset) = object_named(mappings, "preset") {
        if let Some(bindings) = object_named(preset, "group_source_bindings") {
            for entry in bindings {
                if let Value::String(value) = &entry.value {
                    if let Some(source) = value.strip_suffix(" active") {
                        active_groups.insert(entry.key.clone(), source.to_string());
                    }
                }
            }
        }
    }

    let mut bindings = Vec::new();
    for group in objects_named(mappings, "group") {
        let id = string_named(group, "id").unwrap_or_default();
        let mode = string_named(group, "mode").unwrap_or_else(|| "group".into());
        let Some(inputs) = object_named(group, "inputs") else {
            continue;
        };
        let source_group = active_groups
            .get(&id)
            .cloned()
            .unwrap_or_else(|| mode.clone());
        if !active_groups.is_empty() && !active_groups.contains_key(&id) {
            continue;
        }
        for input in inputs {
            let Value::Object(input_body) = &input.value else {
                continue;
            };
            let mut outputs = Vec::new();
            collect_strings_named(input_body, "binding", &mut outputs);
            if outputs.is_empty() {
                continue;
            }
            bindings.push(ControllerBinding {
                source_group: source_group.clone(),
                input: input.key.clone(),
                outputs,
            });
        }
    }
    bindings.sort_by(|a, b| {
        (&a.source_group, &a.input, &a.outputs).cmp(&(&b.source_group, &b.input, &b.outputs))
    });

    Ok(ControllerLayoutMap {
        controller,
        title,
        description,
        controller_type,
        source_file,
        bindings,
    })
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                out.push(Token::Open);
                i += 1;
            }
            b'}' => {
                out.push(Token::Close);
                i += 1;
            }
            b'"' => {
                i += 1;
                let mut value = String::new();
                while i < bytes.len() {
                    match bytes[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' if i + 1 < bytes.len() => {
                            i += 1;
                            value.push(bytes[i] as char);
                            i += 1;
                        }
                        byte => {
                            value.push(byte as char);
                            i += 1;
                        }
                    }
                }
                out.push(Token::String(value));
            }
            _ => {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b'{' | b'}')
                {
                    i += 1;
                }
                out.push(Token::String(
                    String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                ));
            }
        }
    }
    Ok(out)
}

fn parse_entries(tokens: &[Token], cursor: &mut usize, nested: bool) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    while *cursor < tokens.len() {
        if matches!(tokens[*cursor], Token::Close) {
            if !nested {
                bail!("unexpected closing brace");
            }
            *cursor += 1;
            return Ok(entries);
        }
        let Token::String(key) = &tokens[*cursor] else {
            bail!("expected VDF key");
        };
        *cursor += 1;
        let Some(token) = tokens.get(*cursor) else {
            bail!("missing VDF value for {key}");
        };
        let value = match token {
            Token::String(value) => {
                *cursor += 1;
                Value::String(value.clone())
            }
            Token::Open => {
                *cursor += 1;
                Value::Object(parse_entries(tokens, cursor, true)?)
            }
            Token::Close => bail!("missing VDF value for {key}"),
        };
        entries.push(Entry {
            key: key.clone(),
            value,
        });
    }
    if nested {
        bail!("unclosed VDF object");
    }
    Ok(entries)
}

fn object_named<'a>(entries: &'a [Entry], name: &str) -> Option<&'a [Entry]> {
    entries.iter().find_map(|entry| match &entry.value {
        Value::Object(value) if entry.key.eq_ignore_ascii_case(name) => Some(value.as_slice()),
        _ => None,
    })
}

fn objects_named<'a>(entries: &'a [Entry], name: &'a str) -> impl Iterator<Item = &'a [Entry]> {
    entries.iter().filter_map(move |entry| match &entry.value {
        Value::Object(value) if entry.key.eq_ignore_ascii_case(name) => Some(value.as_slice()),
        _ => None,
    })
}

fn string_named(entries: &[Entry], name: &str) -> Option<String> {
    entries.iter().find_map(|entry| match &entry.value {
        Value::String(value) if entry.key.eq_ignore_ascii_case(name) => Some(value.clone()),
        _ => None,
    })
}

fn collect_strings_named(entries: &[Entry], name: &str, out: &mut Vec<String>) {
    for entry in entries {
        match &entry.value {
            Value::String(value) if entry.key.eq_ignore_ascii_case(name) => out.push(value.clone()),
            Value::Object(children) => collect_strings_named(children, name, out),
            Value::String(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_duplicate_keys_and_normalizes_active_groups() {
        let text = r#"
        "controller_mappings"
        {
          "title" "Gamepad"
          "controller_type" "controller_neptune"
          "group"
          {
            "id" "0"
            "mode" "four_buttons"
            "inputs"
            {
              "button_a"
              {
                "bindings"
                {
                  "binding" "xinput_button A, , "
                  "binding" "game_action Jump, , "
                }
              }
            }
          }
          "preset"
          {
            "group_source_bindings"
            {
              "0" "button_diamond active"
            }
          }
        }"#;
        let map = controller_layout_map(text, 0, "layout.vdf".into()).unwrap();
        assert_eq!(map.bindings.len(), 1);
        assert_eq!(map.bindings[0].source_group, "button_diamond");
        assert_eq!(
            map.bindings[0].outputs,
            vec!["xinput_button A, , ", "game_action Jump, , "]
        );
    }
}
