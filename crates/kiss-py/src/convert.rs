//! Moving values between `serde_json` and Python without a JSON string in the
//! middle.
//!
//! A streaming reply produces one event per token. If each of those crossed the
//! boundary as a JSON string that Python then parsed, the parse would cost more
//! than everything else the SDK does. These two functions build native Python
//! containers directly instead, and read them back the same way.

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
use serde_json::{Map, Number, Value};

/// A JSON value that converts into native Python objects.
pub struct Json(pub Value);

impl<'py> IntoPyObject<'py> for Json {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        json_to_py(py, &self.0)
    }
}

/// Build a Python object from a JSON value.
pub fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => py.None().into_bound(py),
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Number(number) => number_to_py(py, number)?,
        Value::String(text) => PyString::new(py, text).into_any(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any()
        }
        Value::Object(entries) => {
            let dict = PyDict::new(py);
            for (key, item) in entries {
                dict.set_item(key, json_to_py(py, item)?)?;
            }
            dict.into_any()
        }
    })
}

fn number_to_py<'py>(py: Python<'py>, number: &Number) -> PyResult<Bound<'py, PyAny>> {
    if let Some(value) = number.as_i64() {
        return Ok(value.into_pyobject(py)?.into_any());
    }
    if let Some(value) = number.as_u64() {
        return Ok(value.into_pyobject(py)?.into_any());
    }
    let value = number.as_f64().unwrap_or(f64::NAN);
    Ok(PyFloat::new(py, value).into_any())
}

/// Build a JSON value from a Python object.
///
/// Accepts `None`, `bool`, `int`, `float`, `str`, and any sequence or mapping of
/// those. Anything else is a `TypeError` naming the offending type, because
/// silently stringifying an unknown object would produce a command the agent
/// cannot act on.
pub fn py_to_json(object: &Bound<'_, PyAny>) -> PyResult<Value> {
    if object.is_none() {
        return Ok(Value::Null);
    }
    // `bool` must be checked before `int`: in Python `True` is an `int`.
    if let Ok(value) = object.cast::<PyBool>() {
        return Ok(Value::Bool(value.is_true()));
    }
    if let Ok(value) = object.cast::<PyString>() {
        return Ok(Value::String(value.to_str()?.to_string()));
    }
    if object.cast::<PyInt>().is_ok() {
        if let Ok(value) = object.extract::<i64>() {
            return Ok(Value::Number(value.into()));
        }
        if let Ok(value) = object.extract::<u64>() {
            return Ok(Value::Number(value.into()));
        }
    }
    if object.cast::<PyFloat>().is_ok() {
        let value: f64 = object.extract()?;
        return Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| PyTypeError::new_err("JSON cannot represent NaN or infinity"));
    }
    if let Ok(mapping) = object.cast::<PyDict>() {
        let mut entries = Map::new();
        for (key, value) in mapping.iter() {
            let key = key
                .extract::<String>()
                .map_err(|_| PyTypeError::new_err("JSON object keys must be strings"))?;
            entries.insert(key, py_to_json(&value)?);
        }
        return Ok(Value::Object(entries));
    }
    if let Ok(items) = object.try_iter() {
        // Strings were handled above, so any remaining iterable is a sequence.
        let mut values = Vec::new();
        for item in items {
            values.push(py_to_json(&item?)?);
        }
        return Ok(Value::Array(values));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert {} to JSON",
        object.get_type().name()?
    )))
}
