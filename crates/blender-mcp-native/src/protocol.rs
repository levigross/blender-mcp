//! Wire framing and envelopes, built on the same crate the server uses.
//!
//! The Python implementation this replaces was a second, independent copy of the format.
//! The two drifted: Python emitted `"result": null` for any Blender call returning
//! `None`, and the server decoded that as an absent field and rejected the envelope.
//! Sharing `blender-mcp-protocol` makes that class of bug unrepresentable.

use std::time::{Duration, Instant};

use blender_mcp_protocol::{
    BridgeError, BridgeResponse, DEFAULT_MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use pyo3::{exceptions::PyTimeoutError, prelude::*, types::PyBytes};
use serde_json::Value;

use crate::{
    errors::ProtocolError,
    marshal::{json_to_py, py_to_json},
};

/// Read exactly `size` bytes from a Python socket, or fail.
fn receive_exactly<'py>(
    connection: &Bound<'py, PyAny>,
    size: usize,
    deadline: Instant,
    nonblocking: bool,
) -> PyResult<std::borrow::Cow<'py, [u8]>> {
    let mut collected: Vec<u8> = Vec::with_capacity(size);
    while collected.len() < size {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| PyTimeoutError::new_err("bridge frame read deadline expired"))?;
        if !nonblocking {
            connection.call_method1("settimeout", (remaining.as_secs_f64(),))?;
        }
        let chunk = connection.call_method1("recv", (size - collected.len(),))?;
        if Instant::now() >= deadline {
            return Err(PyTimeoutError::new_err(
                "bridge frame read deadline expired",
            ));
        }
        let bytes = chunk.cast::<PyBytes>()?.as_bytes().to_vec();
        if bytes.is_empty() {
            return Err(ProtocolError::new_err(
                "connection closed before the frame completed",
            ));
        }
        collected.extend_from_slice(&bytes);
    }
    Ok(std::borrow::Cow::Owned(collected))
}

/// Read one frame within five seconds total, honoring any shorter socket timeout.
#[pyfunction]
pub(crate) fn read_frame<'py>(connection: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let original_timeout = connection.call_method0("gettimeout")?;
    let seconds = original_timeout.extract::<Option<f64>>()?;
    if seconds.is_some_and(|seconds| !seconds.is_finite() || seconds < 0.0) {
        return Err(ProtocolError::new_err(
            "socket timeout must be finite and nonnegative",
        ));
    }
    let nonblocking = seconds == Some(0.0);
    let budget = Duration::from_secs_f64(
        seconds
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(5.0)
            .min(5.0),
    );
    let deadline = Instant::now() + budget;
    let outcome = (|| {
        let header = receive_exactly(connection, 4, deadline, nonblocking)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if length > DEFAULT_MAX_FRAME_BYTES {
            return Err(ProtocolError::new_err(format!(
                "frame declares {length} bytes, exceeding the {DEFAULT_MAX_FRAME_BYTES}-byte limit"
            )));
        }
        let payload = receive_exactly(connection, length, deadline, nonblocking)?;
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|error| ProtocolError::new_err(format!("malformed JSON frame: {error}")))?;
        if !value.is_object() {
            return Err(ProtocolError::new_err(
                "bridge envelope must be a JSON object",
            ));
        }
        json_to_py(connection.py(), &value)
    })();
    let restored = connection.call_method1("settimeout", (&original_timeout,));
    match outcome {
        Ok(value) => {
            restored?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

/// Write one length-prefixed JSON frame to a socket.
#[pyfunction]
pub(crate) fn write_frame(connection: &Bound<'_, PyAny>, value: &Bound<'_, PyAny>) -> PyResult<()> {
    let payload = serde_json::to_vec(&py_to_json(value)?)
        .map_err(|error| ProtocolError::new_err(format!("unserializable response: {error}")))?;
    if payload.len() > DEFAULT_MAX_FRAME_BYTES {
        return Err(ProtocolError::new_err(format!(
            "response is {} bytes, exceeding the {DEFAULT_MAX_FRAME_BYTES}-byte limit",
            payload.len()
        )));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| ProtocolError::new_err("frame length does not fit in u32"))?
            .to_be_bytes(),
    );
    frame.extend_from_slice(&payload);
    connection.call_method1("sendall", (PyBytes::new(connection.py(), &frame),))?;
    Ok(())
}

/// Build a success envelope.
#[pyfunction]
#[pyo3(signature = (request_id, result, *, reports=None, events=None, catalog_revision=None))]
pub(crate) fn success<'py>(
    python: Python<'py>,
    request_id: u64,
    result: &Bound<'py, PyAny>,
    reports: Option<&Bound<'py, PyAny>>,
    events: Option<&Bound<'py, PyAny>>,
    catalog_revision: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let mut response = BridgeResponse::success(request_id, py_to_json(result)?);
    if let Some(reports) = reports
        && let Value::Array(entries) = py_to_json(reports)?
    {
        response.reports = entries
            .into_iter()
            .filter_map(|entry| serde_json::from_value(entry).ok())
            .collect();
    }
    if let Some(events) = events
        && let Value::Array(entries) = py_to_json(events)?
    {
        response.events = entries;
    }
    response.catalog_revision = catalog_revision;
    envelope_to_py(python, &response)
}

/// Build a failure envelope.
#[pyfunction]
#[pyo3(signature = (request_id, code, message, *, data=None, retryable=false, potentially_continuing=false))]
pub(crate) fn failure<'py>(
    python: Python<'py>,
    request_id: u64,
    code: String,
    message: String,
    data: Option<&Bound<'py, PyAny>>,
    retryable: bool,
    potentially_continuing: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let mut error = BridgeError::new(code, message);
    error.retryable = retryable;
    error.potentially_continuing = potentially_continuing;
    error.data = match data {
        Some(value) if !value.is_none() => Some(py_to_json(value)?),
        _ => None,
    };
    envelope_to_py(python, &BridgeResponse::failure(request_id, error))
}

/// Serialize through the shared type so the field set cannot drift from the server's.
fn envelope_to_py<'py>(
    python: Python<'py>,
    response: &BridgeResponse,
) -> PyResult<Bound<'py, PyAny>> {
    let value = serde_json::to_value(response)
        .map_err(|error| ProtocolError::new_err(format!("unserializable envelope: {error}")))?;
    json_to_py(python, &value)
}

/// The wire protocol version, re-exported so Python has a single source for it.
pub(crate) const fn protocol_version() -> u32 {
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use pyo3::types::PyModule;

    use super::*;

    fn socket_type(python: Python<'_>) -> Bound<'_, PyAny> {
        PyModule::from_code(
            python,
            cr#"
import struct
import time

class Socket:
    def __init__(self, delay=0.0, timeout=None):
        payload = b'{"ok": true}'
        self.data = struct.pack('!I', len(payload)) + payload
        self.delay = delay
        self.timeout = timeout
        self.offset = 0

    def gettimeout(self):
        return self.timeout

    def settimeout(self, timeout):
        self.timeout = timeout

    def recv(self, size):
        if self.timeout is not None and self.delay > self.timeout:
            time.sleep(self.timeout)
            raise TimeoutError('socket receive timed out')
        if self.delay:
            time.sleep(self.delay)
        count = min(size, 4 if self.offset == 0 else 1)
        chunk = self.data[self.offset:self.offset + count]
        self.offset += len(chunk)
        return chunk
"#,
            c"frame_test.py",
            c"frame_test",
        )
        .unwrap()
        .getattr("Socket")
        .unwrap()
    }

    #[test]
    fn partial_frame_progress_does_not_restart_the_deadline() {
        Python::initialize();
        Python::attach(|python| {
            let socket = socket_type(python).call1((0.01, 0.04)).unwrap();
            let error = read_frame(&socket).unwrap_err();
            assert!(error.is_instance_of::<PyTimeoutError>(python));
            assert_eq!(
                socket
                    .call_method0("gettimeout")
                    .unwrap()
                    .extract::<f64>()
                    .unwrap()
                    .to_bits(),
                0.04_f64.to_bits()
            );
        });
    }

    #[test]
    fn successful_reads_restore_blocking_and_nonblocking_modes() {
        Python::initialize();
        Python::attach(|python| {
            let class = socket_type(python);
            let blocking = class.call0().unwrap();
            assert!(
                read_frame(&blocking)
                    .unwrap()
                    .get_item("ok")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            assert!(blocking.call_method0("gettimeout").unwrap().is_none());
            let nonblocking = class.call1((0.0, 0.0)).unwrap();
            assert!(read_frame(&nonblocking).is_ok());
            assert_eq!(
                nonblocking
                    .call_method0("gettimeout")
                    .unwrap()
                    .extract::<f64>()
                    .unwrap()
                    .to_bits(),
                0.0_f64.to_bits()
            );
        });
    }
}
