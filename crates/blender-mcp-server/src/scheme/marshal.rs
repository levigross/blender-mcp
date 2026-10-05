use std::fmt::{self, Write as _};

use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use steel::{
    HashMap,
    gc::Gc,
    rerrs::{ErrorKind, SteelErr},
    rvals::{SteelHashMap, SteelString, SteelVal},
};

const MAX_DEPTH: usize = 16;
const MAX_ITEMS: usize = 10_000;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Arguments bound for Blender are limited by the bridge's own request size (2 MiB),
/// not by what is readable in a reply: a node graph or mesh sent in one call must fit.
const MAX_ARGUMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARGUMENT_ITEMS: usize = 100_000;

struct Budget {
    items: usize,
    bytes: usize,
}

impl Budget {
    fn new() -> Self {
        Self {
            items: MAX_ITEMS,
            bytes: MAX_OUTPUT_BYTES,
        }
    }

    const fn for_arguments() -> Self {
        Self {
            items: MAX_ARGUMENT_ITEMS,
            bytes: MAX_ARGUMENT_BYTES,
        }
    }

    fn take(&mut self, bytes: usize) -> Result<(), SteelErr> {
        self.bytes = self
            .bytes
            .checked_sub(bytes)
            .ok_or_else(|| marshal_error("value exceeds the maximum serialization byte count"))?;
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), SteelErr> {
        // Check length before scanning or copying. JSON escaping adds at most five
        // extra bytes per ASCII control character, with quotes and separators too.
        self.take(value.len().saturating_add(3))?;
        for byte in value.bytes() {
            match byte {
                0..=31 => self.take(5)?,
                b'"' | b'\\' => self.take(1)?,
                _ => {}
            }
        }
        Ok(())
    }
}

pub(super) fn json_to_steel(value: &JsonValue) -> Result<SteelVal, SteelErr> {
    let mut remaining = Budget::new();
    json_to_steel_inner(value, 0, &mut remaining)
}

fn json_to_steel_inner(
    value: &JsonValue,
    depth: usize,
    remaining: &mut Budget,
) -> Result<SteelVal, SteelErr> {
    check_budget(depth, remaining)?;
    match value {
        JsonValue::Null => Ok(SteelVal::Void),
        JsonValue::Bool(value) => Ok(SteelVal::BoolV(*value)),
        JsonValue::Number(value) => {
            if let Some(integer) = value.as_i64()
                && let Ok(integer) = isize::try_from(integer)
            {
                return Ok(SteelVal::IntV(integer));
            }
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .map_or_else(
                    || {
                        Err(marshal_error(
                            "JSON number cannot be represented safely in Steel",
                        ))
                    },
                    |number| Ok(SteelVal::NumV(number)),
                )
        }
        JsonValue::String(value) => {
            remaining.string(value)?;
            Ok(SteelVal::StringV(SteelString::from(value.as_str())))
        }
        JsonValue::Array(values) => values
            .iter()
            .map(|value| json_to_steel_inner(value, depth + 1, remaining))
            .collect::<Result<Vec<_>, _>>()
            .map(|values| SteelVal::ListV(values.into())),
        JsonValue::Object(values) => {
            let mut result = HashMap::new();
            for (key, value) in values {
                remaining.string(key)?;
                result.insert(
                    SteelVal::StringV(SteelString::from(key.as_str())),
                    json_to_steel_inner(value, depth + 1, remaining)?,
                );
            }
            Ok(SteelVal::HashMapV(SteelHashMap::from(Gc::new(result))))
        }
    }
}

/// Convert an argument bound for Blender (see `MAX_ARGUMENT_BYTES`).
pub(super) fn steel_to_json(value: &SteelVal) -> Result<JsonValue, SteelErr> {
    let mut remaining = Budget::for_arguments();
    steel_to_json_inner(value, 0, &mut remaining)
}

fn steel_to_json_inner(
    value: &SteelVal,
    depth: usize,
    remaining: &mut Budget,
) -> Result<JsonValue, SteelErr> {
    check_budget(depth, remaining)?;
    match value {
        SteelVal::BoolV(value) => Ok(JsonValue::Bool(*value)),
        SteelVal::NumV(value) => JsonNumber::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| marshal_error("NaN and infinity cannot cross the Blender bridge")),
        SteelVal::IntV(value) => Ok(JsonValue::Number((*value as i64).into())),
        SteelVal::StringV(value) | SteelVal::SymbolV(value) => {
            remaining.string(value.as_ref())?;
            Ok(JsonValue::String(value.to_string()))
        }
        SteelVal::CharV(value) => Ok(JsonValue::String(value.to_string())),
        SteelVal::Void => Ok(JsonValue::Null),
        SteelVal::ListV(values) => values
            .iter()
            .map(|value| steel_to_json_inner(value, depth + 1, remaining))
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        SteelVal::VectorV(values) => values
            .iter()
            .map(|value| steel_to_json_inner(value, depth + 1, remaining))
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        SteelVal::HashMapV(values) => {
            let mut result = JsonMap::new();
            for (key, value) in values.iter() {
                let key = match key {
                    SteelVal::StringV(key) | SteelVal::SymbolV(key) => {
                        remaining.string(key.as_ref())?;
                        key.to_string()
                    }
                    SteelVal::CharV(key) => key.to_string(),
                    _ => {
                        return Err(marshal_error(
                            "only string, symbol, and character map keys can cross the Blender bridge",
                        ));
                    }
                };
                result.insert(key, steel_to_json_inner(value, depth + 1, remaining)?);
            }
            Ok(JsonValue::Object(result))
        }
        _ => Err(marshal_error("unsupported Steel value at Blender boundary")),
    }
}

pub(super) fn values_to_json(values: &[SteelVal]) -> Result<JsonValue, SteelErr> {
    let mut remaining = Budget::new();
    let converted = values
        .iter()
        .map(|value| steel_to_json_inner(value, 0, &mut remaining))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(match converted.as_slice() {
        [] => JsonValue::Null,
        [single] => single.clone(),
        _ => JsonValue::Array(converted),
    })
}

#[cfg(test)]
pub(super) fn values_to_display(values: &[SteelVal]) -> String {
    display_values(values).0
}

struct DisplayWriter {
    output: String,
    items: usize,
    truncated: bool,
}

impl fmt::Write for DisplayWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let room = (MAX_OUTPUT_BYTES - "…[truncated]".len()).saturating_sub(self.output.len());
        let mut end = value.len().min(room);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&value[..end]);
        if end < value.len() {
            self.truncated = true;
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl DisplayWriter {
    fn value(&mut self, value: &SteelVal, depth: usize) -> fmt::Result {
        if depth > MAX_DEPTH || self.items == 0 {
            self.truncated = true;
            return Err(fmt::Error);
        }
        self.items -= 1;
        match value {
            SteelVal::BoolV(value) => self.write_str(if *value { "#true" } else { "#false" }),
            SteelVal::NumV(value) => write!(self, "{value}"),
            SteelVal::IntV(value) => write!(self, "{value}"),
            SteelVal::CharV(value) => write!(self, "#\\{value}"),
            SteelVal::Void => self.write_str("#<void>"),
            SteelVal::SymbolV(value) => self.write_str(value.as_ref()),
            SteelVal::StringV(value) => {
                self.write_char('"')?;
                for ch in value.chars() {
                    match ch {
                        '"' => self.write_str("\\\"")?,
                        '\\' => self.write_str("\\\\")?,
                        '\n' => self.write_str("\\n")?,
                        '\r' => self.write_str("\\r")?,
                        '\t' => self.write_str("\\t")?,
                        _ => self.write_char(ch)?,
                    }
                }
                self.write_char('"')
            }
            SteelVal::ListV(values) => {
                self.write_char('(')?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.write_char(' ')?;
                    }
                    self.value(value, depth + 1)?;
                }
                self.write_char(')')
            }
            SteelVal::VectorV(values) => {
                self.write_str("#(")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.write_char(' ')?;
                    }
                    self.value(value, depth + 1)?;
                }
                self.write_char(')')
            }
            SteelVal::HashMapV(values) => {
                self.write_str("#hash(")?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        self.write_char(' ')?;
                    }
                    self.value(key, depth + 1)?;
                    self.write_char(' ')?;
                    self.value(value, depth + 1)?;
                }
                self.write_char(')')
            }
            // Do not invoke arbitrary recursive Display implementations: custom
            // values may contain cycles, and formatting must remain bounded.
            _ => self.write_str("#<opaque>"),
        }
    }
}

pub(super) fn display_values(values: &[SteelVal]) -> (String, bool) {
    let mut writer = DisplayWriter {
        output: String::new(),
        items: MAX_ITEMS,
        truncated: false,
    };
    for (index, value) in values.iter().enumerate() {
        if (index > 0 && writer.write_char('\n').is_err()) || writer.value(value, 0).is_err() {
            break;
        }
    }
    if writer.truncated {
        writer.output.push_str("…[truncated]");
    }
    (writer.output, writer.truncated)
}

fn check_budget(depth: usize, remaining: &mut Budget) -> Result<(), SteelErr> {
    if depth > MAX_DEPTH {
        return Err(marshal_error(
            "value exceeds the maximum serialization depth",
        ));
    }
    if remaining.items == 0 {
        return Err(marshal_error(
            "value exceeds the maximum serialization item count",
        ));
    }
    remaining.items -= 1;
    remaining.take(32)?;
    Ok(())
}

fn marshal_error(message: impl Into<String>) -> SteelErr {
    SteelErr::new(ErrorKind::Generic, message.into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_round_trip_preserves_bridge_shapes() {
        let expected = json!({
            "operator": "mesh.primitive_cube_add",
            "ok": true,
            "values": [1, 2.5, null, "x"]
        });
        let steel = json_to_steel(&expected).expect("JSON to Steel");
        assert_eq!(steel_to_json(&steel).expect("Steel to JSON"), expected);
    }

    #[test]
    fn rejects_non_finite_numbers() {
        assert!(steel_to_json(&SteelVal::NumV(f64::NAN)).is_err());
    }

    #[test]
    fn arguments_get_the_bridge_budget_and_results_keep_theirs() {
        // A node graph or mesh sent to Blender in one call is bounded by the bridge's
        // 2 MiB request, not by the 10,000 items a readable reply allows.
        let large = SteelVal::ListV((0..20_000).map(SteelVal::IntV).collect());
        assert_eq!(
            steel_to_json(&large)
                .expect("argument converts")
                .as_array()
                .map(Vec::len),
            Some(20_000)
        );
        assert!(values_to_json(std::slice::from_ref(&large)).is_err());
        let too_large = SteelVal::ListV((0..200_000).map(SteelVal::IntV).collect());
        assert!(steel_to_json(&too_large).is_err());
    }

    #[test]
    fn output_limits_are_utf8_safe_and_structured_limits_are_explicit() {
        let values = [SteelVal::StringV("é".repeat(131_071).into())];
        let (display, truncated) = display_values(&values);
        assert!(truncated);
        assert!(display.len() <= MAX_OUTPUT_BYTES);
        assert!(display.ends_with("…[truncated]"));
        assert!(values_to_json(&values).is_err());
    }

    #[test]
    fn many_results_share_one_serialization_budget() {
        let values = vec![SteelVal::IntV(1); MAX_ITEMS + 1];
        assert!(values_to_json(&values).is_err());
        let (display, truncated) = display_values(&values);
        assert!(truncated);
        assert!(display.len() <= MAX_OUTPUT_BYTES);
    }
}
