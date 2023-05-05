use std::fmt;
use std::fmt::{Display, Write};
use std::str::from_utf8;
use std::sync::Arc;

use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::ffi::Py_ssize_t;
use pyo3::once_cell::GILOnceCell;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use pyo3::{ffi, intern};
use serde::ser::{Error, SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

use serde_json::ser::PrettyFormatter;

use crate::build_tools::{py_error_type, safe_repr, SchemaDict};
use crate::errors::LocItem;
use crate::get_version;
use crate::serializers::{SerMode, SerializationState};

use super::line_error::ValLineError;
use super::location::Location;
use super::types::{ErrorMode, ErrorType};
use super::value_exception::PydanticCustomError;
use super::ValError;

#[pyclass(subclass, extends=PyValueError, module="pydantic_core._pydantic_core")]
#[derive(Clone)]
#[cfg_attr(debug_assertions, derive(Debug))]
pub struct ValidationError {
    line_errors: Vec<Arc<PyLineError>>,
    validation_errors: Vec<Arc<ValidationError>>,
    error_mode: ErrorMode,
    #[pyo3(get)]
    message: PyObject,
    #[pyo3(get)]
    loc_prefix: Location,
}

impl ValidationError {
    pub fn new(line_errors: Vec<PyLineError>, message: PyObject, error_mode: ErrorMode) -> Self {
        Self {
            line_errors: line_errors.into_iter().map(Arc::new).collect(),
            validation_errors: Vec::new(),
            message,
            error_mode,
            loc_prefix: Location::Empty,
        }
    }

    pub fn new_with_val_errors(
        line_errors: Vec<Arc<PyLineError>>,
        validation_errors: Vec<Arc<ValidationError>>,
        message: PyObject,
        error_mode: ErrorMode,
        loc_prefix: Location,
    ) -> Self {
        Self {
            line_errors,
            validation_errors,
            message,
            error_mode,
            loc_prefix,
        }
    }

    pub fn from_val_error(
        py: Python,
        message: PyObject,
        error_mode: ErrorMode,
        error: ValError,
        outer_location: Option<LocItem>,
    ) -> PyErr {
        match error {
            ValError::LineErrors(raw_errors) => {
                let line_errors: Vec<PyLineError> = match outer_location {
                    Some(outer_location) => raw_errors
                        .into_iter()
                        .map(|e| e.with_outer_location(outer_location.clone()).into_py(py))
                        .collect(),
                    None => raw_errors.into_iter().map(|e| e.into_py(py)).collect(),
                };
                let validation_error = Self::new(line_errors, message, error_mode);
                match Py::new(py, validation_error) {
                    Ok(err) => PyErr::from_value(err.into_ref(py)),
                    Err(err) => err,
                }
            }
            ValError::InternalErr(err) => err,
            ValError::Omit => Self::omit_error(),
        }
    }

    pub fn display(&self, py: Python, prefix_override: Option<&'static str>) -> String {
        let url_prefix = get_url_prefix(py, include_url_env(py));
        let line_errors = self.flatten_errors();
        let line_errors_string = pretty_py_line_errors(py, &self.error_mode, &line_errors, url_prefix);
        if let Some(prefix) = prefix_override {
            format!("{prefix}\n{line_errors_string}")
        } else {
            let count = line_errors.len();
            let plural = if count == 1 { "" } else { "s" };
            let message: &str = self.message.extract(py).unwrap();
            format!("{count} validation error{plural} for {message}\n{line_errors_string}")
        }
    }

    pub fn omit_error() -> PyErr {
        py_error_type!("Uncaught Omit error, please check your usage of `default` validators.")
    }
}

static URL_ENV_VAR: GILOnceCell<bool> = GILOnceCell::new();

fn _get_include_url_env() -> bool {
    match std::env::var("PYDANTIC_ERRORS_OMIT_URL") {
        Ok(val) => val.is_empty(),
        Err(_) => true,
    }
}

fn include_url_env(py: Python) -> bool {
    *URL_ENV_VAR.get_or_init(py, _get_include_url_env)
}

static URL_PREFIX: GILOnceCell<String> = GILOnceCell::new();

fn get_url_prefix(py: Python, include_url: bool) -> Option<&str> {
    if include_url {
        Some(URL_PREFIX.get_or_init(py, || format!("https://errors.pydantic.dev/{}/v/", get_version())))
    } else {
        None
    }
}

// used to convert a validation error back to ValError for wrap functions
impl<'a> IntoPy<ValError<'a>> for ValidationError {
    fn into_py(self, py: Python) -> ValError<'a> {
        self.flatten_errors()
            .into_iter()
            .map(|e| e.into_py(py))
            .collect::<Vec<_>>()
            .into()
    }
}

type LineErrors = Vec<Arc<PyLineError>>;
type ValidationErrors = Vec<Arc<ValidationError>>;

fn partition_errors(errors: &PyList) -> PyResult<(LineErrors, ValidationErrors)> {
    let mut line_errors = Vec::new();
    let mut validation_errors = Vec::new();

    for py_exc in errors {
        // Note: can we use a Py<> instead of an Arc to avoid the clone?
        if let Ok(e) = py_exc.extract() {
            validation_errors.push(Arc::new(e))
        } else {
            let e = PyLineError::try_from(py_exc)?;
            line_errors.push(Arc::new(e))
        }
    }
    Ok((line_errors, validation_errors))
}

#[pymethods]
impl ValidationError {
    #[pyo3(signature = (message, errors, error_mode = None, *_args, loc_prefix = None, **_kwargs))]
    #[new]
    fn py_new(
        message: PyObject,
        errors: &PyList,
        error_mode: Option<&str>,
        _args: &PyTuple,
        loc_prefix: Option<&PyAny>,
        _kwargs: Option<&PyDict>,
    ) -> PyResult<Self> {
        let (line_errors, validation_errors) = partition_errors(errors)?;
        Ok(Self {
            line_errors,
            validation_errors,
            message,
            error_mode: error_mode.try_into()?,
            loc_prefix: loc_prefix.try_into()?,
        })
    }

    #[getter]
    fn title(&self) -> PyObject {
        self.message.clone()
    }

    pub fn flatten_errors(&self) -> Vec<PyLineError> {
        let mut res: Vec<_> = self.line_errors.iter().map(|v| (**v).clone()).collect();
        for v in &self.validation_errors {
            res.extend(v.flatten_errors().iter().map(|v| v.with_prefix(&self.loc_prefix)));
        }
        res
    }

    pub fn error_count(&self) -> usize {
        let mut res = self.line_errors.len();
        for v in &self.validation_errors {
            res += v.error_count()
        }
        res
    }

    #[pyo3(signature = (*, include_url = true, include_context = true))]
    pub fn errors(&self, py: Python, include_url: bool, include_context: bool) -> PyResult<Py<PyList>> {
        let line_errors = self.flatten_errors();
        let url_prefix = get_url_prefix(py, include_url);
        // taken approximately from the pyo3, but modified to return the error during iteration
        // https://github.com/PyO3/pyo3/blob/a3edbf4fcd595f0e234c87d4705eb600a9779130/src/types/list.rs#L27-L55
        unsafe {
            let ptr = ffi::PyList_New(line_errors.len() as Py_ssize_t);

            // We create the `Py` pointer here for two reasons:
            // - panics if the ptr is null
            // - its Drop cleans up the list if user code or the asserts panic.
            let list: Py<PyList> = Py::from_owned_ptr(py, ptr);

            for (index, line_error) in (0_isize..).zip(&line_errors) {
                let item = line_error.as_dict(py, url_prefix, include_context, &self.error_mode)?;
                ffi::PyList_SET_ITEM(ptr, index, item.into_ptr());
            }

            Ok(list)
        }
    }

    #[pyo3(signature = (*, indent = None, include_url = true, include_context = true))]
    pub fn json<'py>(
        &self,
        py: Python<'py>,
        indent: Option<usize>,
        include_url: bool,
        include_context: bool,
    ) -> PyResult<&'py PyString> {
        let state = SerializationState::new(None, None);
        let extra = state.extra(py, &SerMode::Json, true, false, false, true, None);
        let line_errors = self.flatten_errors();
        let serializer = ValidationErrorSerializer {
            py,
            line_errors: &line_errors,
            url_prefix: get_url_prefix(py, include_url),
            include_context,
            extra: &extra,
            error_mode: &self.error_mode,
        };

        let writer: Vec<u8> = Vec::with_capacity(line_errors.len() * 200);
        let bytes = match indent {
            Some(indent) => {
                let indent = vec![b' '; indent];
                let formatter = PrettyFormatter::with_indent(&indent);
                let mut ser = serde_json::Serializer::with_formatter(writer, formatter);
                serializer.serialize(&mut ser).map_err(json_py_err)?;
                ser.into_inner()
            }
            None => {
                let mut ser = serde_json::Serializer::new(writer);
                serializer.serialize(&mut ser).map_err(json_py_err)?;
                ser.into_inner()
            }
        };
        let s = from_utf8(&bytes).map_err(json_py_err)?;
        Ok(PyString::new(py, s))
    }

    fn derive(&self, py: Python, errors: &PyList) -> PyResult<Py<Self>> {
        let (mut line_errors, mut validation_errors) = partition_errors(errors)?;
        line_errors.extend(self.line_errors.iter().cloned());
        validation_errors.extend(self.validation_errors.iter().cloned());
        Py::new(
            py,
            Self::new_with_val_errors(
                line_errors,
                validation_errors,
                self.message.clone(),
                self.error_mode.clone(),
                self.loc_prefix.clone(),
            ),
        )
    }

    fn __repr__(&self, py: Python) -> String {
        self.display(py, None)
    }

    fn __str__(&self, py: Python) -> String {
        self.__repr__(py)
    }
}

macro_rules! truncate_input_value {
    ($out:expr, $value:expr) => {
        if $value.len() > 50 {
            write!(
                $out,
                ", input_value={}...{}",
                &$value[0..25],
                &$value[$value.len() - 24..]
            )?;
        } else {
            write!($out, ", input_value={}", $value)?;
        }
    };
}

pub fn pretty_py_line_errors(
    py: Python,
    error_mode: &ErrorMode,
    line_errors_iter: &[PyLineError],
    url_prefix: Option<&str>,
) -> String {
    line_errors_iter
        .iter()
        .map(|i| i.pretty(py, error_mode, url_prefix))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|err| vec![format!("[error formatting line errors: {err}]")])
        .join("\n")
}

/// `PyLineError` are the public version of `ValLineError`, as help and used in `ValidationError`s
#[pyclass]
#[derive(Clone)]
#[cfg_attr(debug_assertions, derive(Debug))]
pub struct PyLineError {
    error_type: ErrorType,
    location: Location,
    input_value: PyObject,
}

impl<'a> IntoPy<PyLineError> for ValLineError<'a> {
    fn into_py(self, py: Python<'_>) -> PyLineError {
        PyLineError {
            error_type: self.error_type,
            location: self.location,
            input_value: self.input_value.to_object(py),
        }
    }
}

/// opposite of above, used to extract line errors from a validation error for wrap functions
impl<'a> IntoPy<ValLineError<'a>> for PyLineError {
    fn into_py(self, _py: Python) -> ValLineError<'a> {
        ValLineError {
            error_type: self.error_type,
            location: self.location,
            input_value: self.input_value.into(),
        }
    }
}

impl TryFrom<&PyAny> for PyLineError {
    type Error = PyErr;

    fn try_from(value: &PyAny) -> PyResult<Self> {
        let dict: &PyDict = value.downcast()?;
        let py = value.py();

        let type_raw = dict
            .get_item(intern!(py, "type"))
            .ok_or_else(|| PyKeyError::new_err("type"))?;

        let error_type = if let Ok(type_str) = type_raw.downcast::<PyString>() {
            let context: Option<&PyDict> = dict.get_as(intern!(py, "ctx"))?;
            ErrorType::new(py, type_str.to_str()?, context)?
        } else if let Ok(custom_error) = type_raw.extract::<PydanticCustomError>() {
            ErrorType::new_custom_error(custom_error)
        } else {
            return Err(PyTypeError::new_err(
                "`type` should be a `str` or `PydanticCustomError`",
            ));
        };

        let location = Location::try_from(dict.get_item("loc"))?;

        let input_value = match dict.get_item("input") {
            Some(i) => i.into_py(py),
            None => py.None(),
        };

        Ok(Self {
            error_type,
            location,
            input_value,
        })
    }
}

impl PyLineError {
    pub fn with_prefix(&self, prefix: &Location) -> PyLineError {
        PyLineError {
            error_type: self.error_type.clone(),
            location: self.location.with_prefix(prefix),
            input_value: self.input_value.clone(),
        }
    }

    fn get_error_url(&self, url_prefix: &str) -> String {
        format!("{url_prefix}{}", self.error_type.type_string())
    }

    pub fn as_dict(
        &self,
        py: Python,
        url_prefix: Option<&str>,
        include_context: bool,
        error_mode: &ErrorMode,
    ) -> PyResult<PyObject> {
        let dict = PyDict::new(py);
        dict.set_item("type", self.error_type.type_string())?;
        dict.set_item("loc", self.location.to_object(py))?;
        dict.set_item("msg", self.error_type.render_message(py, error_mode)?)?;
        dict.set_item("input", &self.input_value)?;
        if include_context {
            if let Some(context) = self.error_type.py_dict(py)? {
                dict.set_item("ctx", context)?;
            }
        }
        if let Some(url_prefix) = url_prefix {
            match self.error_type {
                ErrorType::CustomError { custom_error: _ } => {
                    // Don't add URLs for custom errors
                }
                _ => {
                    dict.set_item("url", self.get_error_url(url_prefix))?;
                }
            }
        }
        Ok(dict.into_py(py))
    }

    fn pretty(&self, py: Python, error_mode: &ErrorMode, url_prefix: Option<&str>) -> Result<String, fmt::Error> {
        let mut output = String::with_capacity(200);
        write!(output, "{}", self.location)?;

        let message = match self.error_type.render_message(py, error_mode) {
            Ok(message) => message,
            Err(err) => format!("(error rendering message: {err})"),
        };
        write!(output, "  {message} [type={}", self.error_type.type_string())?;

        let input_value = self.input_value.as_ref(py);
        let input_str = safe_repr(input_value);
        truncate_input_value!(output, input_str);

        if let Ok(type_) = input_value.get_type().name() {
            write!(output, ", input_type={type_}")?;
        }
        if let Some(url_prefix) = url_prefix {
            match self.error_type {
                ErrorType::CustomError { custom_error: _ } => {
                    // Don't display URLs for custom errors
                    output.push(']');
                }
                _ => {
                    write!(
                        output,
                        "]\n    For further information visit {}",
                        self.get_error_url(url_prefix)
                    )?;
                }
            }
        } else {
            output.push(']');
        }
        Ok(output)
    }
}

pub(super) fn json_py_err(error: impl Display) -> PyErr {
    PyValueError::new_err(format!("Error serializing ValidationError to JSON: {error}"))
}

pub(super) fn py_err_json<S>(error: PyErr) -> S::Error
where
    S: Serializer,
{
    S::Error::custom(error.to_string())
}

struct ValidationErrorSerializer<'py> {
    py: Python<'py>,
    line_errors: &'py [PyLineError],
    url_prefix: Option<&'py str>,
    include_context: bool,
    extra: &'py crate::serializers::Extra<'py>,
    error_mode: &'py ErrorMode,
}

impl<'py> Serialize for ValidationErrorSerializer<'py> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.line_errors.len()))?;
        for line_error in self.line_errors {
            let line_s = PyLineErrorSerializer {
                py: self.py,
                line_error,
                url_prefix: self.url_prefix,
                include_context: self.include_context,
                extra: self.extra,
                error_mode: self.error_mode,
            };
            seq.serialize_element(&line_s)?;
        }
        seq.end()
    }
}

struct PyLineErrorSerializer<'py> {
    py: Python<'py>,
    line_error: &'py PyLineError,
    url_prefix: Option<&'py str>,
    include_context: bool,
    extra: &'py crate::serializers::Extra<'py>,
    error_mode: &'py ErrorMode,
}

impl<'py> Serialize for PyLineErrorSerializer<'py> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let py = self.py;
        let mut size = 4;
        if self.url_prefix.is_some() {
            size += 1;
        }
        if self.include_context {
            size += 1;
        }
        let mut map = serializer.serialize_map(Some(size))?;

        map.serialize_entry("type", &self.line_error.error_type.type_string())?;

        map.serialize_entry("loc", &self.line_error.location)?;

        let msg = self
            .line_error
            .error_type
            .render_message(py, self.error_mode)
            .map_err(py_err_json::<S>)?;
        map.serialize_entry("msg", &msg)?;

        map.serialize_entry(
            "input",
            &self.extra.serialize_infer(self.line_error.input_value.as_ref(py)),
        )?;

        if self.include_context {
            if let Some(context) = self.line_error.error_type.py_dict(py).map_err(py_err_json::<S>)? {
                map.serialize_entry("ctx", &self.extra.serialize_infer(context.as_ref(py)))?;
            }
        }
        if let Some(url_prefix) = self.url_prefix {
            map.serialize_entry("url", &self.line_error.get_error_url(url_prefix))?;
        }
        map.end()
    }
}
