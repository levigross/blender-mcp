# SPDX-License-Identifier: GPL-3.0-or-later
"""Bounded main-thread execution with independent socket-thread controls."""

from __future__ import annotations

import json
import hashlib
import os
import queue
import socket
import threading
import time
import traceback
import uuid
from collections import OrderedDict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import bpy

from .operations import BlenderOperations, OperationError
from .protocol import PROTOCOL_VERSION, ProtocolError, failure, read_frame, success, write_frame
from .scheme_blender_mcp_native import __version__ as NATIVE_VERSION

TERMINAL = frozenset({"succeeded", "failed", "cancelled", "expired"})
WORK_QUEUE_LIMIT = 256
CONNECTION_LIMIT = 128
WAITER_LIMIT = 32
REGISTRY_LIMIT = 512
RETENTION_SECONDS = 300
CACHE_BYTES = 8 * 1024 * 1024
RESULT_BYTES = 1024 * 1024
REQUEST_BYTES = 2 * 1024 * 1024
PENDING_REQUEST_BYTES = 16 * 1024 * 1024
BATCH_SLICE_MS = 20
# Live dispatch runs inside Blender's timer. A sequential client sends its next
# request within a fraction of a millisecond of the previous reply, so waiting a full
# idle interval per request made every bridge call cost ~20 ms. While requests keep
# arriving, serve them back to back within a slice, then yield briefly to the UI.
IDLE_INTERVAL = 0.02
BUSY_INTERVAL = 0.001
BUSY_SLICE = 0.016
NEXT_REQUEST_WAIT = 0.002
BUSY_LINGER = 0.5
CAPABILITIES = ["request_receipts", "queued_deadlines", "control_status", "render_jobs", "reference_epochs", "batch"]


class BridgeBusyError(Exception):
    """A render job owns the dispatcher; controls remain available."""


@dataclass
class WorkItem:
    request: dict[str, Any]
    expires: float
    completed: threading.Event = field(default_factory=threading.Event)
    response: dict[str, Any] | None = None
    cached_response: dict[str, Any] | None = None
    state: str = "queued"
    cancel_requested: bool = False
    finished: float | None = None
    job_id: str | None = None
    cache_size: int = 0
    batch_results: list[dict[str, Any]] = field(default_factory=list)
    batch_index: int = 0
    started: float | None = None
    submitted: float = field(default_factory=time.monotonic)
    generation: int | None = None
    request_size: int = 0
    admitted_bytes: int = 0
    execution_seconds: float = 0.0
    dispatch_started: float | None = None
    response_bytes: int = 0
    cache_serialization_seconds: float = 0.0
    batch_bytes: int = 0
    waiter_detached: bool = False


class BridgeServer:
    def __init__(self, host: str, port: int, *, quit_on_shutdown: bool = False):
        resolved = socket.gethostbyname(host)
        if not resolved.startswith("127."):
            raise ValueError("the Blender bridge may only bind to IPv4 loopback")
        self.host, self.port = resolved, port
        self.quit_on_shutdown = quit_on_shutdown
        self.operations = BlenderOperations()
        self.instance_id = os.environ.get("BLENDER_MCP_INSTANCE_ID") or uuid.uuid4().hex
        # Cache these on the main thread; control requests never call bpy/native RNA.
        self.generation = self.operations.generation
        self.blender_version = bpy.app.version_string
        self.work: queue.Queue[WorkItem] = queue.Queue(maxsize=WORK_QUEUE_LIMIT)
        self.registry: OrderedDict[tuple[str, int], WorkItem] = OrderedDict()
        self.jobs: dict[str, tuple[str, int]] = {}
        self.early_cancellations: OrderedDict[tuple[str, int], float] = OrderedDict()
        self.lock = threading.RLock()
        self.connections = threading.BoundedSemaphore(CONNECTION_LIMIT)
        self.waiters = threading.BoundedSemaphore(WAITER_LIMIT)
        self.stop_event = threading.Event()
        # Set whenever work is queued, so the dispatcher can wake at once.
        self.work_available = threading.Event()
        self.last_work = 0.0
        self.ready = threading.Event()
        self.thread: threading.Thread | None = None
        self.listener: socket.socket | None = None
        self.active: WorkItem | None = None
        self.active_request: int | None = None
        self.last_request = ""
        self.last_error = ""
        self.connection_count = 0
        self.invalidation_pending = False
        self.subdata_dirty = False
        self.executing_native = False
        self.catalog_revision = ""
        self.build_id = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        self.request_count = 0
        self.completed_count = 0
        self.queue_wait_seconds = 0.0
        self.execution_seconds = 0.0
        self.request_bytes = 0
        self.response_bytes = 0
        self.cache_serialization_seconds = 0.0

    @property
    def address(self) -> tuple[str, int]:
        return self.listener.getsockname() if self.listener else (self.host, self.port)

    def start(self) -> None:
        if self.thread and self.thread.is_alive():
            return
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind((self.host, self.port))
            listener.listen(32)
            listener.settimeout(0.2)
        except BaseException:
            listener.close()
            self.operations.close()
            raise
        self.listener = listener
        self.port = listener.getsockname()[1]
        self.stop_event.clear()
        self.ready.set()
        self.thread = threading.Thread(target=self._socket_loop, name="blender-mcp-bridge", daemon=True)
        try:
            self.thread.start()
        except BaseException:
            self.thread = None
            self.listener = None
            self.ready.clear()
            listener.close()
            self.operations.close()
            raise

    def stop(self) -> None:
        self.stop_event.set()
        with self.lock:
            for item in self.registry.values():
                if item.state == "queued":
                    self._finish(item, "cancelled", failure(item.request["id"], "bridge_stopped", "bridge stopped before execution"))
                elif item.state == "running":
                    item.cancel_requested = True
            listener, self.listener = self.listener, None
        if listener:
            listener.close()
        if self.thread and self.thread is not threading.current_thread():
            self.thread.join(timeout=1)
        self.thread = None
        self.ready.clear()

    def _socket_loop(self) -> None:
        listener = self.listener
        assert listener is not None
        while not self.stop_event.is_set():
            try:
                connection, peer = listener.accept()
            except TimeoutError:
                continue
            except OSError:
                break
            if not peer[0].startswith("127.") or not self.connections.acquire(blocking=False):
                connection.close()
                continue
            self.connection_count += 1
            threading.Thread(target=self._handle_connection, args=(connection,), daemon=True).start()

    @staticmethod
    def _key(request: dict[str, Any]) -> tuple[str, int]:
        session = request.get("session_id", "")
        if not isinstance(session, str) or not session or len(session) > 128:
            raise ValueError("session_id must be a nonempty string of at most 128 characters")
        identifier = request.get("id")
        if type(identifier) is not int or not 0 <= identifier < 2**64:
            raise ValueError("id must be an unsigned 64-bit integer")
        return session, identifier

    def _prune(self) -> None:
        now = time.monotonic()
        for key, expires in list(self.early_cancellations.items()):
            if expires < now:
                self.early_cancellations.pop(key)
        for key, item in list(self.registry.items()):
            if item.state == "queued" and now >= item.expires:
                self._finish(item, "expired", failure(item.request["id"], "deadline_exceeded", "deadline expired before execution"))
            if item.finished is not None and (now - item.finished > RETENTION_SECONDS or len(self.registry) >= REGISTRY_LIMIT):
                self.registry.pop(key)
                if item.job_id:
                    self.jobs.pop(item.job_id, None)
        total = sum(item.cache_size for item in self.registry.values())
        for item in self.registry.values():
            if total <= CACHE_BYTES:
                break
            if item.cache_size:
                total -= item.cache_size
                item.cache_size = 0
                item.cached_response = None

    def _finish(self, item: WorkItem, state: str, response: dict[str, Any]) -> None:
        self.completed_count += 1
        item.state = state
        item.response = None if item.waiter_detached else response
        item.finished = time.monotonic()
        response.setdefault("events", []).append({
            "kind": "mcp_request", "request_id": item.request["id"],
            "session_id": item.request["session_id"], "instance_id": self.instance_id,
            "operation": item.request["operation"], "state": state,
            "elapsed_seconds": max(0, item.finished - (item.started or item.submitted)),
            "completed_commands": item.batch_index if item.request["operation"] == "batch" else int(state == "succeeded"),
            **self._request_metrics(item, item.finished),
        })
        serialization_started = time.monotonic()
        size = len(json.dumps(response).encode())
        item.cache_serialization_seconds = max(0, time.monotonic() - serialization_started)
        item.response_bytes = size
        self.cache_serialization_seconds += item.cache_serialization_seconds
        self.response_bytes += size
        if size <= RESULT_BYTES:
            item.cached_response, item.cache_size = response, size
        item.request = {key: item.request[key] for key in ("id", "session_id", "operation")}
        item.request_size = 0
        item.batch_results = []
        item.completed.set()

    @staticmethod
    def _request_metrics(item: WorkItem, now: float) -> dict[str, Any]:
        queue_end = item.started if item.started is not None else item.finished if item.finished is not None else now
        active_seconds = max(0, now - item.dispatch_started) if item.dispatch_started is not None else 0
        return {
            "queue_wait_seconds": max(0, queue_end - item.submitted),
            "execution_seconds": item.execution_seconds + active_seconds,
            "admitted_request_bytes": item.admitted_bytes,
        }

    def _snapshot(self, item: WorkItem) -> dict[str, Any]:
        now = time.monotonic()
        return {
            "instance_id": self.instance_id, "session_id": item.request["session_id"],
            "request_id": item.request["id"], "job_id": item.job_id, "state": item.state,
            "operation": item.request["operation"],
            "cancel_requested": item.cancel_requested,
            "potentially_continuing": item.state == "running",
            "result_available": item.cached_response is not None,
            "completed_commands": item.batch_index if item.request["operation"] == "batch" else int(item.state == "succeeded"),
            "elapsed_seconds": max(0, (item.finished or now) - (item.started or item.submitted)),
            "deadline_exceeded": now >= item.expires and item.state not in TERMINAL,
            **self._request_metrics(item, now),
            "response_bytes": item.response_bytes,
            "cached_response_bytes": item.cache_size,
            "cache_serialization_seconds": item.cache_serialization_seconds,
        }

    def _control(self, request: dict[str, Any]) -> dict[str, Any] | None:
        operation, identifier = request.get("operation"), request["id"]
        with self.lock:
            self._prune()
            if operation == "control_status":
                return success(identifier, {
                    "instance_id": self.instance_id, "protocol_version": PROTOCOL_VERSION,
                    "extension_version": "0.1.0", "native_version": NATIVE_VERSION,
                    "build_id": self.build_id,
                    "capabilities": CAPABILITIES, "blender_version": self.blender_version,
                    "generation": self.generation, "catalog_revision": self.catalog_revision,
                    "state": "stopping" if self.stop_event.is_set() else "busy" if self.active else "ready",
                    "active": self._snapshot(self.active) if self.active else None,
                    "queued": sum(item.state == "queued" for item in self.registry.values()),
                    "retention_seconds": RETENTION_SECONDS,
                    "limits": {
                        "work_queue": WORK_QUEUE_LIMIT, "connections": CONNECTION_LIMIT,
                        "synchronous_waiters": WAITER_LIMIT, "request_registry": REGISTRY_LIMIT,
                        "request_bytes": REQUEST_BYTES, "pending_request_bytes": PENDING_REQUEST_BYTES,
                        "result_cache_bytes": CACHE_BYTES, "cached_response_bytes": RESULT_BYTES,
                        "batch_slice_ms": BATCH_SLICE_MS, "batch_result_bytes": RESULT_BYTES,
                    },
                    "byte_accounting": "bridge_json_utf8",
                    "journal_scope": "mcp_requests_only",
                    "recent_requests": [self._snapshot(item) for item in list(self.registry.values())[-16:]],
                    "metrics": {"requests": self.request_count, "completed": self.completed_count,
                                "queue_wait_seconds": self.queue_wait_seconds,
                                "execution_seconds": self.execution_seconds, "request_bytes": self.request_bytes,
                                "response_bytes": self.response_bytes,
                                "cache_serialization_seconds": self.cache_serialization_seconds},
                })
            if operation == "shutdown":
                self.stop()
                return success(identifier, {"shutting_down": True, "potentially_continuing": self.active is not None})
            if operation not in {"cancel", "request_status", "request_result", "job_status", "job_result", "job_cancel"}:
                return None
            if operation.startswith("job_"):
                key = self.jobs.get(request.get("job_id", ""))
            else:
                key = (request.get("target_session") or request["session_id"], request.get("request_id"))
            item = self.registry.get(key)
            if item is None:
                if (operation == "cancel" and isinstance(key[0], str) and 0 < len(key[0]) <= 128
                        and type(key[1]) is int and 0 <= key[1] < 2**64):
                    # The control connection can overtake the original connection.
                    # A short bounded tombstone prevents that later arrival executing.
                    self.early_cancellations[key] = time.monotonic() + 10
                    while len(self.early_cancellations) > REGISTRY_LIMIT:
                        self.early_cancellations.popitem(last=False)
                return failure(identifier, "receipt_unavailable", "request or job is unknown or outside the retention window")
            if operation in {"cancel", "job_cancel"}:
                if item.state == "queued":
                    item.cancel_requested = True
                    self._finish(item, "cancelled", failure(item.request["id"], "cancelled", "cancelled before execution"))
                elif item.state == "running":
                    item.cancel_requested = True
                return success(identifier, self._snapshot(item))
            if operation in {"request_status", "job_status"}:
                return success(identifier, self._snapshot(item))
            if item.state not in TERMINAL:
                return failure(identifier, "result_pending", "execution has not finished", data=self._snapshot(item), retryable=True, potentially_continuing=item.state == "running")
            if item.cached_response is None:
                return failure(identifier, "result_evicted", "result exceeds the cache budget or was evicted", data=self._snapshot(item))
            if operation == "request_result":
                return success(identifier, {**self._snapshot(item), "response": item.cached_response})
            response = dict(item.cached_response)
            response["id"] = identifier
            return response

    def _enqueue(self, request: dict[str, Any]) -> WorkItem:
        key = self._key(request)
        with self.lock:
            self._prune()
            if self.stop_event.is_set():
                raise ValueError("bridge is stopping")
            if key in self.registry:
                raise ValueError("duplicate request id in this session; retrieve the existing receipt")
            if any(entry.job_id and entry.state not in TERMINAL for entry in self.registry.values()):
                raise BridgeBusyError("a render job owns the dispatcher; inspect job-status before submitting more Blender work")
            if len(self.registry) >= REGISTRY_LIMIT:
                raise queue.Full
            # Both endpoints are on this host; convert its wall-clock deadline once.
            deadline = request.get("deadline_unix_ms")
            if deadline is not None and (type(deadline) is not int or not 0 <= deadline < 2**64):
                raise ValueError("deadline_unix_ms must be an unsigned integer")
            duration = 120.0 if deadline is None else (deadline / 1000 - time.time())
            if request.get("operation") == "render_start":
                seconds = request.get("timeout_secs")
                if seconds is not None and (type(seconds) is not int or not 0 < seconds <= 3600):
                    raise ValueError("render timeout_secs must be between 1 and 3600")
                if duration > 0:
                    duration = seconds or 3600
            size = len(json.dumps(request).encode())
            if size > REQUEST_BYTES or sum(entry.request_size for entry in self.registry.values()) + size > PENDING_REQUEST_BYTES:
                raise queue.Full
            item = WorkItem(dict(request), time.monotonic() + max(0, min(duration, 3600)))
            item.request_size = size
            item.admitted_bytes = size
            if request.get("operation") == "render_start":
                item.job_id = uuid.uuid4().hex
            cancelled_early = key in self.early_cancellations
            if not cancelled_early:
                self.work.put_nowait(item)
                self.work_available.set()
            self.registry[key] = item
            self.request_count += 1
            self.request_bytes += size
            if item.job_id:
                self.jobs[item.job_id] = key
            if cancelled_early:
                self.early_cancellations.pop(key)
                item.cancel_requested = True
                self._finish(item, "cancelled", failure(request["id"], "cancelled", "cancelled before admission"))
            return item

    def _handle_connection(self, connection: socket.socket) -> None:
        identifier = 0
        acquired_waiter = False
        item = None
        try:
            connection.settimeout(5)
            request = read_frame(connection)
            identifier = request.get("id", 0)
            self._key(request)
            if request.get("version") != PROTOCOL_VERSION:
                write_frame(connection, failure(identifier, "version_mismatch", f"expected protocol {PROTOCOL_VERSION}"))
                return
            expected = request.get("expected_instance")
            if expected and expected != self.instance_id:
                write_frame(connection, failure(identifier, "instance_changed", "Blender bridge instance changed", data={"instance_id": self.instance_id}))
                return
            response = self._control(request)
            if response is None:
                asynchronous = request.get("operation") == "render_start"
                if not asynchronous:
                    acquired_waiter = self.waiters.acquire(blocking=False)
                    if not acquired_waiter:
                        raise queue.Full
                item = self._enqueue(request)
                if asynchronous:
                    response = success(identifier, self._snapshot(item))
                else:
                    while not item.completed.wait(timeout=0.1):
                        with self.lock:
                            if self.stop_event.is_set() and item.state == "running":
                                response = failure(identifier, "bridge_stopped", "bridge stopped while execution may still continue", data=self._snapshot(item), potentially_continuing=True)
                                break
                            if item.state == "queued" and time.monotonic() >= item.expires:
                                self._finish(item, "expired", failure(identifier, "deadline_exceeded", "deadline expired before execution"))
                    else:
                        response = item.response
            write_frame(connection, response)
        except BridgeBusyError as error:
            self._write_error(connection, identifier, "bridge_busy", str(error), retryable=True)
        except queue.Full:
            self._write_error(connection, identifier, "queue_full", "bridge execution capacity is full")
        except (OSError, ProtocolError, ValueError, TypeError) as error:
            self.last_error = str(error)
            self._write_error(connection, identifier, "protocol_error", str(error))
        finally:
            if item and not item.job_id:
                with self.lock:
                    item.waiter_detached = True
                    item.response = None
            if acquired_waiter:
                self.waiters.release()
            connection.close()
            self.connections.release()

    @staticmethod
    def _write_error(connection: socket.socket, identifier: int, code: str, message: str, *, retryable: bool = False) -> None:
        try:
            write_frame(connection, failure(identifier, code, message, retryable=retryable))
        except (OSError, ProtocolError):
            pass

    def invalidate(self, *_unused: object) -> None:
        # Handlers may run inside native execute; do not borrow it reentrantly.
        self.invalidation_pending = True

    def _apply_invalidation(self) -> None:
        if self.invalidation_pending:
            self.generation = self.operations.invalidate_references()
            self.invalidation_pending = False
            self.subdata_dirty = False
        elif self.subdata_dirty:
            self.operations.invalidate_subdata("a geometry or shading update in Blender")
            self.subdata_dirty = False

    def _execute(self, request: dict[str, Any]) -> tuple[Any, Any]:
        self.executing_native = True
        try:
            return self.operations.execute(request)
        finally:
            self.executing_native = False
            self._apply_invalidation()

    def _batch_slice(self, item: WorkItem) -> tuple[str, dict[str, Any]] | None:
        requests = item.request["requests"]
        if item.batch_index == 0:
            self.operations.validate_batch(requests)
        started = time.monotonic()
        slice_seconds = max(1, min(item.request.get("max_elapsed_ms", BATCH_SLICE_MS), BATCH_SLICE_MS)) / 1000
        while item.batch_index < len(requests):
            if item.generation != self.generation:
                return "failed", failure(item.request["id"], "stale_reference", "document changed between batch commands", data={"completed": item.batch_index, "results": item.batch_results, "complete": False})
            if item.cancel_requested or time.monotonic() >= item.expires or self.stop_event.is_set():
                reason = "cancelled" if item.cancel_requested else "deadline_exceeded"
                return "cancelled" if item.cancel_requested else "expired", failure(item.request["id"], reason, "batch stopped between commands; completed edits remain", data={"completed": item.batch_index, "results": item.batch_results, "complete": False})
            index = item.batch_index
            try:
                result, reports = self._execute(requests[index])
            except Exception as error:
                details = {"code": getattr(error, "code", "blender_error"), "message": str(error)}
                if getattr(error, "data", None) is not None:
                    details["data"] = error.data
                item.batch_results.append({"index": index, "error": details})
                return "failed", success(item.request["id"], {"completed": index, "results": item.batch_results, "complete": False, "failed_index": index, "error": details})
            item.batch_index += 1
            entry = {"index": index, "result": result}
            if reports:
                entry["reports"] = reports
            item.batch_bytes += len(json.dumps(entry).encode())
            if item.batch_bytes > RESULT_BYTES:
                return "failed", failure(item.request["id"], "serialization_limit", "batch result budget exceeded after a completed command; edits remain", data={"completed": item.batch_index, "results": item.batch_results, "omitted_result_index": index, "complete": False})
            item.batch_results.append(entry)
            if time.monotonic() - started >= slice_seconds:
                return None
        return "succeeded", success(item.request["id"], {"completed": item.batch_index, "results": item.batch_results, "complete": True})

    def dispatch_once(self) -> bool:
        self._apply_invalidation()
        item = self.active
        if item is None:
            try:
                item = self.work.get_nowait()
            except queue.Empty:
                return False
            with self.lock:
                if item.state in TERMINAL:
                    self.work.task_done()
                    return True
                identifier = item.request["id"]
                expected = item.request.get("expected_generation")
                if time.monotonic() >= item.expires:
                    self._finish(item, "expired", failure(identifier, "deadline_exceeded", "deadline expired before execution"))
                    self.work.task_done()
                    return True
                if expected is not None and expected != self.generation:
                    self._finish(item, "failed", failure(identifier, "stale_reference", "document changed while request was queued"))
                    self.work.task_done()
                    return True
                item.state = "running"
                item.started = time.monotonic()
                item.generation = self.generation
                self.queue_wait_seconds += item.started - item.submitted
                self.active = item
                self.active_request = identifier
        identifier = item.request["id"]
        request = dict(item.request)
        if item.job_id:
            request["operation"], request["write_still"] = "render", True
        self.last_request = str(request.get("operation", ""))
        execution_start = time.monotonic()
        item.dispatch_started = execution_start
        try:
            if request.get("operation") == "batch":
                outcome = self._batch_slice(item)
                if outcome is None:
                    return True
                state, response = outcome
            else:
                result, reports = self._execute(request)
                self.catalog_revision = self.operations.catalog_revision
                response = success(identifier, result, reports=reports, catalog_revision=self.catalog_revision)
                state = "succeeded"
        except OperationError as error:
            self.last_error = str(error)
            response = failure(identifier, error.code, str(error), data=error.data)
            state = "failed"
        except Exception as error:
            self.last_error = traceback.format_exc(limit=8)
            response = failure(identifier, "blender_error", str(error), data={"traceback": self.last_error})
            state = "failed"
        finally:
            with self.lock:
                executed = max(0, time.monotonic() - execution_start)
                item.execution_seconds += executed
                self.execution_seconds += executed
                item.dispatch_started = None
        # Publish the new epoch before releasing the reply to the caller; its next
        # control handshake must not observe the old epoch after a successful load.
        self._apply_invalidation()
        with self.lock:
            self._finish(item, state, response)
            if item.job_id:
                item.response = None
            self.active = None
            self.active_request = None
            self.work.task_done()
            self._prune()
        return True

    def timer_tick(self) -> float | None:
        global _SERVER
        if self.stop_event.is_set():
            if self.active is not None:
                self.dispatch_once()
            self.stop()
            self.operations.close()
            if _SERVER is self:
                _SERVER = None
                _hooks(False)
            return None
        started = time.monotonic()
        while time.monotonic() - started < BUSY_SLICE:
            if self.dispatch_once():
                self.last_work = time.monotonic()
            elif time.monotonic() - self.last_work >= BUSY_LINGER or not self.wait_for_work(NEXT_REQUEST_WAIT):
                # Idle: never hold the main thread waiting for work that is not coming.
                break
        busy = time.monotonic() - self.last_work < BUSY_LINGER
        return BUSY_INTERVAL if busy else IDLE_INTERVAL

    def wait_for_work(self, timeout: float) -> bool:
        """Block up to `timeout` until a request is queued; True if one may be waiting."""
        self.work_available.clear()
        if self.active is not None or not self.work.empty():
            return True
        return self.work_available.wait(timeout)

    def run_headless(self) -> None:
        self.start()
        try:
            while not self.stop_event.is_set():
                if not self.dispatch_once():
                    self.wait_for_work(0.01)
        finally:
            if self.active is not None:
                self.dispatch_once()
            self.stop()
            self.operations.close()


_SERVER: BridgeServer | None = None


def get_server() -> BridgeServer | None:
    return _SERVER


def _lifecycle(_unused: object) -> None:
    if _SERVER is not None:
        _SERVER.invalidate()


_lifecycle = bpy.app.handlers.persistent(_lifecycle)


def _depsgraph(_scene: object, depsgraph: Any) -> None:
    if _SERVER is not None and not _SERVER.executing_native:
        if any(update.is_updated_geometry or update.is_updated_shading for update in depsgraph.updates):
            _SERVER.subdata_dirty = True


_depsgraph = bpy.app.handlers.persistent(_depsgraph)


def _hooks(install: bool) -> None:
    for name, callback in (("load_pre", _lifecycle), ("undo_pre", _lifecycle), ("redo_pre", _lifecycle), ("depsgraph_update_post", _depsgraph)):
        handlers = getattr(bpy.app.handlers, name)
        if install and callback not in handlers:
            handlers.append(callback)
        elif not install and callback in handlers:
            handlers.remove(callback)


def start_live(port: int = 9876) -> BridgeServer:
    global _SERVER
    if _SERVER is not None:
        return _SERVER
    server = BridgeServer("127.0.0.1", port)
    server.start()
    _SERVER = server
    try:
        _hooks(True)
        bpy.app.timers.register(server.timer_tick, first_interval=IDLE_INTERVAL, persistent=True)
    except BaseException:
        _hooks(False)
        _SERVER = None
        server.stop()
        server.operations.close()
        raise
    return server


def stop_live() -> None:
    global _SERVER
    if _SERVER is not None:
        server, _SERVER = _SERVER, None
        if bpy.app.timers.is_registered(server.timer_tick):
            bpy.app.timers.unregister(server.timer_tick)
        server.stop()
        if server.active is not None:
            server.dispatch_once()
        server.operations.close()
    _hooks(False)


def run_headless(host: str, port: int) -> None:
    global _SERVER
    server = BridgeServer(host, port, quit_on_shutdown=True)
    _SERVER = server
    _hooks(True)
    try:
        server.run_headless()
    finally:
        _hooks(False)
        _SERVER = None
