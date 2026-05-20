use super::types::Reducer;
use anyhow::{Result, bail};
use serde_json::{Number, Value};

/// Combines a branch's incoming write with the current state value (if any)
/// via the specified reducer. The result is what gets written back to live
/// state during the super-step merge phase.
///
/// `current = None` means the key has no prior value in this super-step or in
/// live state. Most reducers treat absent as their identity (empty array,
/// empty string, no prior value). `Overwrite` ignores `current` entirely.
///
/// Errors clearly when types are incompatible with the reducer (e.g.
/// `Sum` on a string), naming the reducer and which side (`current` / `incoming`)
/// has the wrong type.
pub fn apply(reducer: Reducer, current: Option<&Value>, incoming: Value) -> Result<Value> {
    match reducer {
        Reducer::Append => apply_append(current, incoming),
        Reducer::Extend => apply_extend(current, incoming),
        Reducer::Concat => apply_concat(current, incoming),
        Reducer::Sum => apply_sum(current, incoming),
        Reducer::Max => apply_max(current, incoming),
        Reducer::Min => apply_min(current, incoming),
        Reducer::Merge => apply_merge(current, incoming),
        Reducer::Overwrite => Ok(incoming),
    }
}

fn apply_append(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let mut arr = match current {
        None => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(other) => bail!(
            "reducer 'append' requires an array (or absent) for the current value, got {}",
            type_name(other)
        ),
    };
    arr.push(incoming);
    Ok(Value::Array(arr))
}

fn apply_extend(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let mut arr = match current {
        None => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(other) => bail!(
            "reducer 'extend' requires an array (or absent) for the current value, got {}",
            type_name(other)
        ),
    };
    match incoming {
        Value::Array(items) => arr.extend(items),
        other => bail!(
            "reducer 'extend' requires an array for the incoming value, got {}",
            type_name(&other)
        ),
    }
    Ok(Value::Array(arr))
}

fn apply_concat(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let incoming_str = match incoming {
        Value::String(s) => s,
        other => bail!(
            "reducer 'concat' requires a string for the incoming value, got {}",
            type_name(&other)
        ),
    };
    let result = match current {
        None => incoming_str,
        Some(Value::String(c)) => {
            if c.is_empty() {
                incoming_str
            } else {
                format!("{c}\n{incoming_str}")
            }
        }
        Some(other) => bail!(
            "reducer 'concat' requires a string (or absent) for the current value, got {}",
            type_name(other)
        ),
    };
    Ok(Value::String(result))
}

fn apply_sum(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let i = number_or_error(&incoming, "sum", "incoming")?;
    let c = match current {
        None => 0.0,
        Some(value) => number_or_error(value, "sum", "current")?,
    };
    Ok(json_number(c + i))
}

fn apply_max(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let i = number_or_error(&incoming, "max", "incoming")?;
    match current {
        None => Ok(json_number(i)),
        Some(value) => {
            let c = number_or_error(value, "max", "current")?;
            Ok(json_number(c.max(i)))
        }
    }
}

fn apply_min(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let i = number_or_error(&incoming, "min", "incoming")?;
    match current {
        None => Ok(json_number(i)),
        Some(value) => {
            let c = number_or_error(value, "min", "current")?;
            Ok(json_number(c.min(i)))
        }
    }
}

fn apply_merge(current: Option<&Value>, incoming: Value) -> Result<Value> {
    let mut map = match current {
        None => serde_json::Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(other) => bail!(
            "reducer 'merge' requires an object (or absent) for the current value, got {}",
            type_name(other)
        ),
    };
    match incoming {
        Value::Object(items) => {
            for (k, v) in items {
                map.insert(k, v);
            }
        }
        other => bail!(
            "reducer 'merge' requires an object for the incoming value, got {}",
            type_name(&other)
        ),
    }
    Ok(Value::Object(map))
}

fn number_or_error(value: &Value, reducer_name: &str, position: &str) -> Result<f64> {
    match value.as_f64() {
        Some(n) => Ok(n),
        None => bail!(
            "reducer '{reducer_name}' requires a number for the {position} value, got {}",
            type_name(value)
        ),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// Numeric reducers compute in f64 for simplicity. We preserve integer typing
// when the result is losslessly representable as i64 so `count: sum` stays an
// integer rather than degrading to a float. Non-finite values (NaN, Inf) can't
// arise from finite inputs to +/max/min, so the fallback never fires in practice.
fn json_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.is_finite() && n.abs() <= (i64::MAX as f64) {
        Value::Number(Number::from(n as i64))
    } else {
        match Number::from_f64(n) {
            Some(num) => Value::Number(num),
            None => Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_to_absent_creates_single_element_array() {
        let result = apply(Reducer::Append, None, json!("a")).unwrap();

        assert_eq!(result, json!(["a"]));
    }

    #[test]
    fn append_pushes_onto_existing_array() {
        let current = json!(["a", "b"]);
        let result = apply(Reducer::Append, Some(&current), json!("c")).unwrap();

        assert_eq!(result, json!(["a", "b", "c"]));
    }

    #[test]
    fn append_errors_when_current_is_not_array() {
        let current = json!("not an array");

        let err = apply(Reducer::Append, Some(&current), json!("x"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'append'"), "got: {err}");
        assert!(err.contains("string"), "got: {err}");
    }

    #[test]
    fn extend_concatenates_arrays() {
        let current = json!([1, 2]);

        let result = apply(Reducer::Extend, Some(&current), json!([3, 4])).unwrap();

        assert_eq!(result, json!([1, 2, 3, 4]));
    }

    #[test]
    fn extend_from_absent_with_array() {
        let result = apply(Reducer::Extend, None, json!([1, 2])).unwrap();

        assert_eq!(result, json!([1, 2]));
    }

    #[test]
    fn extend_errors_when_incoming_is_not_array() {
        let err = apply(Reducer::Extend, None, json!(42))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'extend'"), "got: {err}");
        assert!(err.contains("number"), "got: {err}");
        assert!(err.contains("incoming"), "got: {err}");
    }

    #[test]
    fn concat_joins_strings_with_newline() {
        let current = json!("first");

        let result = apply(Reducer::Concat, Some(&current), json!("second")).unwrap();

        assert_eq!(result, json!("first\nsecond"));
    }

    #[test]
    fn concat_from_absent_yields_incoming() {
        let result = apply(Reducer::Concat, None, json!("hello")).unwrap();

        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn concat_skips_separator_when_current_is_empty_string() {
        let current = json!("");

        let result = apply(Reducer::Concat, Some(&current), json!("first")).unwrap();

        assert_eq!(result, json!("first"));
    }

    #[test]
    fn concat_errors_when_incoming_is_not_string() {
        let err = apply(Reducer::Concat, None, json!(42))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'concat'"), "got: {err}");
        assert!(err.contains("number"), "got: {err}");
    }

    #[test]
    fn sum_adds_numbers() {
        let current = json!(5);

        let result = apply(Reducer::Sum, Some(&current), json!(7)).unwrap();

        assert_eq!(result, json!(12));
    }

    #[test]
    fn sum_starts_from_zero_when_current_absent() {
        let result = apply(Reducer::Sum, None, json!(3.5)).unwrap();

        assert_eq!(result, json!(3.5));
    }

    #[test]
    fn sum_preserves_integer_type_for_whole_results() {
        let current = json!(2);

        let result = apply(Reducer::Sum, Some(&current), json!(3)).unwrap();

        assert!(result.is_i64(), "expected integer, got {result:?}");
        assert_eq!(result, json!(5));
    }

    #[test]
    fn sum_uses_float_when_result_has_fractional() {
        let current = json!(1.5);
        let result = apply(Reducer::Sum, Some(&current), json!(2.25)).unwrap();

        assert_eq!(result, json!(3.75));
    }

    #[test]
    fn sum_errors_on_string_incoming() {
        let err = apply(Reducer::Sum, None, json!("not a number"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'sum'"), "got: {err}");
        assert!(err.contains("string"), "got: {err}");
    }

    #[test]
    fn max_returns_larger_of_two() {
        let current = json!(5);
        let result = apply(Reducer::Max, Some(&current), json!(3)).unwrap();
        assert_eq!(result, json!(5));

        let result = apply(Reducer::Max, Some(&current), json!(10)).unwrap();
        assert_eq!(result, json!(10));
    }

    #[test]
    fn max_yields_incoming_when_current_absent() {
        let result = apply(Reducer::Max, None, json!(42)).unwrap();

        assert_eq!(result, json!(42));
    }

    #[test]
    fn min_returns_smaller_of_two() {
        let current = json!(5);
        let result = apply(Reducer::Min, Some(&current), json!(3)).unwrap();
        assert_eq!(result, json!(3));

        let result = apply(Reducer::Min, Some(&current), json!(10)).unwrap();
        assert_eq!(result, json!(5));
    }

    #[test]
    fn min_errors_on_non_numeric_current() {
        let current = json!("oops");

        let err = apply(Reducer::Min, Some(&current), json!(1))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'min'"), "got: {err}");
        assert!(err.contains("current"), "got: {err}");
    }

    #[test]
    fn merge_unions_objects_with_incoming_winning_collisions() {
        let current = json!({ "a": 1, "b": 2 });
        let incoming = json!({ "b": 99, "c": 3 });

        let result = apply(Reducer::Merge, Some(&current), incoming).unwrap();

        assert_eq!(result, json!({ "a": 1, "b": 99, "c": 3 }));
    }

    #[test]
    fn merge_from_absent_yields_incoming_object() {
        let result = apply(Reducer::Merge, None, json!({ "k": "v" })).unwrap();

        assert_eq!(result, json!({ "k": "v" }));
    }

    #[test]
    fn merge_errors_when_incoming_is_not_object() {
        let err = apply(Reducer::Merge, None, json!([1, 2]))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'merge'"), "got: {err}");
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn merge_errors_when_current_is_not_object() {
        let current = json!("not object");

        let err = apply(Reducer::Merge, Some(&current), json!({ "k": "v" }))
            .unwrap_err()
            .to_string();

        assert!(err.contains("'merge'"), "got: {err}");
        assert!(err.contains("current"), "got: {err}");
    }

    #[test]
    fn overwrite_ignores_current_and_returns_incoming() {
        let current = json!("old");

        let result = apply(Reducer::Overwrite, Some(&current), json!("new")).unwrap();

        assert_eq!(result, json!("new"));
    }

    #[test]
    fn overwrite_works_with_absent_current() {
        let result = apply(Reducer::Overwrite, None, json!(42)).unwrap();

        assert_eq!(result, json!(42));
    }
}
