use anyhow::{anyhow, Result};

pub(super) const TUI_PLUGIN_SPEC: &str = "./luvus-tui.mjs";
const LEGACY_TUI_PLUGIN_SPEC: &str = "./luvus-tui.js";
const MAX_TUI_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct RootObject {
    close: usize,
    properties: usize,
    trailing_comma: bool,
    plugin: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct ArrayElement {
    start: usize,
    end: usize,
    comma_after: Option<usize>,
}

struct ArrayValue {
    close: usize,
    elements: Vec<ArrayElement>,
}

fn skip_trivia(input: &[u8], cursor: &mut usize) -> Result<()> {
    loop {
        while input.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
        if input.get(*cursor..*cursor + 2) == Some(b"//") {
            *cursor += 2;
            while input.get(*cursor).is_some_and(|byte| *byte != b'\n') {
                *cursor += 1;
            }
            continue;
        }
        if input.get(*cursor..*cursor + 2) == Some(b"/*") {
            *cursor += 2;
            while input.get(*cursor..*cursor + 2) != Some(b"*/") {
                if *cursor >= input.len() {
                    return Err(anyhow!("invalid OpenCode tui config: unclosed comment"));
                }
                *cursor += 1;
            }
            *cursor += 2;
            continue;
        }
        return Ok(());
    }
}

fn string_end(input: &[u8], start: usize) -> Result<usize> {
    if input.get(start) != Some(&b'"') {
        return Err(anyhow!("invalid OpenCode tui config: expected string"));
    }
    let mut cursor = start + 1;
    while let Some(byte) = input.get(cursor) {
        match byte {
            b'"' => return Ok(cursor + 1),
            b'\\' => {
                cursor += 2;
                if cursor > input.len() {
                    break;
                }
            }
            byte if *byte < 0x20 => break,
            _ => cursor += 1,
        }
    }
    Err(anyhow!("invalid OpenCode tui config: unclosed string"))
}

fn composite_end(input: &[u8], start: usize) -> Result<usize> {
    let first = *input
        .get(start)
        .ok_or_else(|| anyhow!("invalid OpenCode tui config: missing value"))?;
    let mut stack = vec![match first {
        b'[' => b']',
        b'{' => b'}',
        _ => return Err(anyhow!("invalid OpenCode tui config: expected container")),
    }];
    let mut cursor = start + 1;
    while cursor < input.len() {
        match input[cursor] {
            b'"' => cursor = string_end(input, cursor)?,
            b'/' if input.get(cursor + 1) == Some(&b'/') => {
                cursor += 2;
                while input.get(cursor).is_some_and(|byte| *byte != b'\n') {
                    cursor += 1;
                }
            }
            b'/' if input.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while input.get(cursor..cursor + 2) != Some(b"*/") {
                    if cursor >= input.len() {
                        return Err(anyhow!("invalid OpenCode tui config: unclosed comment"));
                    }
                    cursor += 1;
                }
                cursor += 2;
            }
            b'[' => {
                stack.push(b']');
                cursor += 1;
            }
            b'{' => {
                stack.push(b'}');
                cursor += 1;
            }
            b']' | b'}' => {
                if stack.pop() != Some(input[cursor]) {
                    return Err(anyhow!("invalid OpenCode tui config: mismatched container"));
                }
                cursor += 1;
                if stack.is_empty() {
                    return Ok(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    Err(anyhow!("invalid OpenCode tui config: unclosed container"))
}

fn value_end(input: &[u8], start: usize) -> Result<usize> {
    match input.get(start) {
        Some(b'"') => string_end(input, start),
        Some(b'[' | b'{') => composite_end(input, start),
        Some(_) => {
            let mut cursor = start;
            while input.get(cursor).is_some_and(|byte| {
                !byte.is_ascii_whitespace()
                    && !matches!(byte, b',' | b']' | b'}')
                    && !(matches!(byte, b'/') && matches!(input.get(cursor + 1), Some(b'/' | b'*')))
            }) {
                cursor += 1;
            }
            if cursor == start {
                Err(anyhow!("invalid OpenCode tui config: missing value"))
            } else {
                Ok(cursor)
            }
        }
        None => Err(anyhow!("invalid OpenCode tui config: missing value")),
    }
}

fn decode_string(input: &str, start: usize, end: usize) -> Option<String> {
    serde_json::from_str(&input[start..end]).ok()
}

fn root_object(input: &str) -> Result<RootObject> {
    if input.len() > MAX_TUI_CONFIG_BYTES {
        return Err(anyhow!("OpenCode tui config exceeds 1 MiB"));
    }
    let bytes = input.as_bytes();
    let mut cursor = 0;
    skip_trivia(bytes, &mut cursor)?;
    if bytes.get(cursor) != Some(&b'{') {
        return Err(anyhow!("OpenCode tui config must contain an object"));
    }
    cursor += 1;
    let mut properties = 0;
    let mut trailing_comma = false;
    let mut plugin = None;
    loop {
        skip_trivia(bytes, &mut cursor)?;
        if bytes.get(cursor) == Some(&b'}') {
            let close = cursor;
            cursor += 1;
            skip_trivia(bytes, &mut cursor)?;
            if cursor != bytes.len() {
                return Err(anyhow!("invalid OpenCode tui config: trailing content"));
            }
            return Ok(RootObject {
                close,
                properties,
                trailing_comma,
                plugin,
            });
        }

        let key_start = cursor;
        let key_end = string_end(bytes, key_start)?;
        let key = decode_string(input, key_start, key_end)
            .ok_or_else(|| anyhow!("invalid OpenCode tui config: invalid property name"))?;
        cursor = key_end;
        skip_trivia(bytes, &mut cursor)?;
        if bytes.get(cursor) != Some(&b':') {
            return Err(anyhow!("invalid OpenCode tui config: expected colon"));
        }
        cursor += 1;
        skip_trivia(bytes, &mut cursor)?;
        let start = cursor;
        let end = value_end(bytes, start)?;
        if key == "plugin" && plugin.replace((start, end)).is_some() {
            return Err(anyhow!(
                "OpenCode tui config contains duplicate plugin properties"
            ));
        }
        properties += 1;
        cursor = end;
        skip_trivia(bytes, &mut cursor)?;
        match bytes.get(cursor) {
            Some(b',') => {
                trailing_comma = true;
                cursor += 1;
            }
            Some(b'}') => trailing_comma = false,
            _ => return Err(anyhow!("invalid OpenCode tui config: expected comma")),
        }
    }
}

fn array_value(input: &str, start: usize, end: usize) -> Result<ArrayValue> {
    let bytes = input.as_bytes();
    if bytes.get(start) != Some(&b'[') || bytes.get(end.saturating_sub(1)) != Some(&b']') {
        return Err(anyhow!(
            "OpenCode tui config `plugin` must be an array before Luvus can edit it"
        ));
    }
    let close = end - 1;
    let mut cursor = start + 1;
    let mut elements = Vec::new();
    loop {
        skip_trivia(bytes, &mut cursor)?;
        if cursor == close {
            return Ok(ArrayValue { close, elements });
        }
        if cursor > close {
            return Err(anyhow!("invalid OpenCode tui config plugin array"));
        }
        let element_start = cursor;
        let element_end = value_end(bytes, element_start)?;
        cursor = element_end;
        skip_trivia(bytes, &mut cursor)?;
        let comma_after = if bytes.get(cursor) == Some(&b',') {
            let comma = cursor;
            cursor += 1;
            Some(comma)
        } else if cursor == close {
            None
        } else {
            return Err(anyhow!("invalid OpenCode tui config plugin array"));
        };
        elements.push(ArrayElement {
            start: element_start,
            end: element_end,
            comma_after,
        });
    }
}

fn plugin_elements(input: &str) -> Result<(RootObject, ArrayValue)> {
    let root = root_object(input)?;
    let (start, end) = root
        .plugin
        .ok_or_else(|| anyhow!("OpenCode tui config has no plugin array"))?;
    Ok((root, array_value(input, start, end)?))
}

fn plugin_name(input: &str, element: ArrayElement) -> Option<String> {
    decode_string(input, element.start, element.end)
}

fn remove_element(input: &str, array: &ArrayValue, index: usize) -> String {
    let element = array.elements[index];
    let (start, end) = if let Some(comma) = element.comma_after {
        (element.start, comma + 1)
    } else if index > 0 {
        (
            array.elements[index - 1]
                .comma_after
                .expect("a preceding array element has a comma"),
            element.end,
        )
    } else {
        (element.start, element.end)
    };
    let mut output = input.to_string();
    output.replace_range(start..end, "");
    output
}

fn add_plugin(input: &str) -> Result<String> {
    let root = root_object(input)?;
    let mut output = input.to_string();
    if root.plugin.is_none() {
        let separator = if root.properties == 0 || root.trailing_comma {
            ""
        } else {
            ","
        };
        output.insert_str(
            root.close,
            &format!("{separator}\n  \"plugin\": [\"{TUI_PLUGIN_SPEC}\"]\n"),
        );
        return Ok(output);
    }
    let (_, array) = plugin_elements(input)?;
    if let Some(last) = array.elements.last() {
        if last.comma_after.is_some() {
            output.insert_str(array.close, &format!("\"{TUI_PLUGIN_SPEC}\""));
        } else {
            output.insert_str(last.end, &format!(", \"{TUI_PLUGIN_SPEC}\""));
        }
    } else {
        output.insert_str(array.close, &format!("\"{TUI_PLUGIN_SPEC}\""));
    }
    Ok(output)
}

pub(super) fn enable(input: &str) -> Result<String> {
    let mut output = input.to_string();
    loop {
        let root = root_object(&output)?;
        if root.plugin.is_none() {
            return add_plugin(&output);
        }
        let (_, array) = plugin_elements(&output)?;
        let mut current = Vec::new();
        let mut legacy = None;
        for (index, element) in array.elements.iter().copied().enumerate() {
            match plugin_name(&output, element).as_deref() {
                Some(TUI_PLUGIN_SPEC) => current.push(index),
                Some(LEGACY_TUI_PLUGIN_SPEC) => legacy = Some(index),
                _ => {}
            }
        }
        if let Some(index) = legacy {
            output = remove_element(&output, &array, index);
        } else if current.len() > 1 {
            output = remove_element(&output, &array, *current.last().unwrap());
        } else if current.len() == 1 {
            return Ok(output);
        } else {
            return add_plugin(&output);
        }
    }
}

pub(super) fn disable(input: &str) -> Result<String> {
    let mut output = input.to_string();
    loop {
        let root = root_object(&output)?;
        let Some((start, end)) = root.plugin else {
            return Ok(output);
        };
        let array = array_value(&output, start, end)?;
        let Some(index) = array.elements.iter().copied().position(|element| {
            matches!(
                plugin_name(&output, element).as_deref(),
                Some(TUI_PLUGIN_SPEC | LEGACY_TUI_PLUGIN_SPEC)
            )
        }) else {
            return Ok(output);
        };
        output = remove_element(&output, &array, index);
    }
}

pub(super) fn enabled(input: &str) -> bool {
    plugin_elements(input).is_ok_and(|(_, array)| {
        array
            .elements
            .iter()
            .copied()
            .any(|element| plugin_name(input, element).as_deref() == Some(TUI_PLUGIN_SPEC))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_plugin_entry_without_losing_jsonc_comments_or_neighbors() {
        let original = r#"{
  // preserve me
  "theme": "tokyonight",
  "plugin": ["other-plugin",],
}
"#;
        let installed = enable(original).unwrap();
        assert!(installed.contains("// preserve me"));
        assert!(installed.contains("other-plugin"));
        assert!(enabled(&installed));
        assert_eq!(enable(&installed).unwrap(), installed);

        let removed = disable(&installed).unwrap();
        assert!(!enabled(&removed));
        assert!(removed.contains("// preserve me"));
        assert!(removed.contains("other-plugin"));
    }

    #[test]
    fn migrates_owned_legacy_and_duplicate_entries_only() {
        let original = r#"{
  "plugin": [
    "./luvus-tui.js",
    "other",
    "./luvus-tui.mjs",
    "./luvus-tui.mjs",
  ]
}"#;
        let installed = enable(original).unwrap();
        assert_eq!(installed.matches(TUI_PLUGIN_SPEC).count(), 1);
        assert!(!installed.contains(LEGACY_TUI_PLUGIN_SPEC));
        assert!(installed.contains("other"));
        assert!(root_object(&installed).is_ok());

        let removed = disable(&installed).unwrap();
        assert!(!removed.contains(TUI_PLUGIN_SPEC));
        assert!(removed.contains("other"));
        assert!(root_object(&removed).is_ok());
    }

    #[test]
    fn adds_a_missing_plugin_property_to_empty_and_populated_objects() {
        for input in [
            "{}\n",
            "{ // note\n}\n",
            "{\"theme\":\"x\"}\n",
            "{\"mouse\":true /* keep, and } here */,\"theme\":\"x\"}\n",
        ] {
            let output = enable(input).unwrap();
            assert!(enabled(&output), "{output}");
            assert!(root_object(&output).is_ok(), "{output}");
        }
    }

    #[test]
    fn refuses_ambiguous_or_unbounded_configuration() {
        assert!(enable(r#"{"plugin":"keep-me"}"#).is_err());
        assert!(enable(r#"{"plugin":[],"plugin":[]}"#).is_err());
        assert!(enable(&format!(
            "{{\"padding\":\"{}\"}}",
            "x".repeat(MAX_TUI_CONFIG_BYTES)
        ))
        .is_err());
    }
}
