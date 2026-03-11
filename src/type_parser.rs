use cel_core::CelType;

/// Parse a celsp type string into a `CelType`.
///
/// Supports:
/// - Primitives: bool, int, uint, double, string, bytes
/// - Special: null, dyn, timestamp, duration, error
/// - Parameterized: list(T), map(K, V), optional(T), type(T), wrapper(T)
/// - Message types: any.other.name
pub fn parse_type_string(s: &str) -> Result<CelType, String> {
    let s = s.trim();

    if let Some(inner_start) = s.find('(') {
        if !s.ends_with(')') {
            return Err(format!(
                "malformed type string: missing closing paren in '{}'",
                s
            ));
        }

        let type_name = &s[..inner_start];
        let inner = &s[inner_start + 1..s.len() - 1];

        return match type_name {
            "list" => {
                let elem = parse_type_string(inner)?;
                Ok(CelType::list(elem))
            }
            "map" => {
                let (key_str, val_str) = split_map_types(inner)?;
                let key = parse_type_string(key_str)?;
                let val = parse_type_string(val_str)?;
                Ok(CelType::map(key, val))
            }
            "optional" => {
                let elem = parse_type_string(inner)?;
                Ok(CelType::optional(elem))
            }
            "type" => {
                let elem = parse_type_string(inner)?;
                Ok(CelType::type_of(elem))
            }
            "wrapper" => {
                let elem = parse_type_string(inner)?;
                Ok(CelType::wrapper(elem))
            }
            _ => Err(format!("unknown parameterized type: '{}'", type_name)),
        };
    }

    match s {
        "bool" => Ok(CelType::Bool),
        "int" => Ok(CelType::Int),
        "uint" => Ok(CelType::UInt),
        "double" => Ok(CelType::Double),
        "string" => Ok(CelType::String),
        "bytes" => Ok(CelType::Bytes),
        "null" => Ok(CelType::Null),
        "dyn" => Ok(CelType::Dyn),
        "timestamp" => Ok(CelType::Timestamp),
        "duration" => Ok(CelType::Duration),
        "error" => Ok(CelType::Error),
        "" => Err("empty type string".to_string()),
        _ => Ok(CelType::message(s)),
    }
}

fn split_map_types(s: &str) -> Result<(&str, &str), String> {
    let mut depth = 0;
    let mut split_pos = None;

    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Err(format!("unbalanced parentheses in map type: '{}'", s));
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                if split_pos.is_some() {
                    return Err(format!("map type has more than 2 parameters: '{}'", s));
                }
                split_pos = Some(i);
            }
            _ => {}
        }
    }

    if depth != 0 {
        return Err(format!("unbalanced parentheses in map type: '{}'", s));
    }

    match split_pos {
        Some(pos) => Ok((s[..pos].trim(), s[pos + 1..].trim())),
        None => Err(format!("map type must have 2 parameters: '{}'", s)),
    }
}
