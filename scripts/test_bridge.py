"""Exercise bridge lifecycle and controls without a graphical Blender process."""

import importlib.util
import json
import os
import socket
import struct
import sys
import threading
import time
import types
import unittest
from pathlib import Path
from unittest.mock import patch


class OperationError(Exception):
    def __init__(self, code, message, data=None):
        super().__init__(message)
        self.code, self.data = code, data


class FakeOperations:
    def __init__(self):
        self.generation = 1
        self.catalog_revision = "catalog"
        self.executed = []
        self.entered = threading.Event()
        self.release = threading.Event()
        self.closed = False
        self.subdata_invalidations = 0

    def execute(self, request):
        self.executed.append(request)
        if hasattr(self, "on_execute"):
            self.on_execute()
        if request.get("blocking"):
            self.entered.set()
            if not self.release.wait(2):
                raise RuntimeError("test did not release operation")
        if request.get("operation") == "fail":
            raise OperationError("test_failure", "deliberate failure", {"field": "test"})
        if request.get("delay"):
            time.sleep(request["delay"])
        if request.get("operation") == "render":
            return {"artifact": {"id": "frame", "path": "/tmp/frame.png"}}, []
        return {"ok": True}, []

    def validate_batch(self, requests):
        if not isinstance(requests, list) or not 0 < len(requests) <= 100:
            raise OperationError("invalid_arguments", "invalid batch")
        if any(request.get("operation") == "shutdown" for request in requests):
            raise OperationError("invalid_arguments", "forbidden batch operation")

    def invalidate_references(self):
        self.generation += 1
        return self.generation

    def close(self):
        self.closed = True

    def invalidate_subdata(self):
        self.subdata_invalidations += 1


def read_exact(connection, size):
    result = b""
    while len(result) < size:
        chunk = connection.recv(size - len(result))
        if not chunk:
            raise OSError("closed frame")
        result += chunk
    return result


def read_frame(connection):
    size = struct.unpack("!I", read_exact(connection, 4))[0]
    return json.loads(read_exact(connection, size))


def write_frame(connection, value):
    payload = json.dumps(value).encode()
    connection.sendall(struct.pack("!I", len(payload)) + payload)


def success(identifier, result, **kwargs):
    return {"version": 1, "id": identifier, "result": result, **kwargs}


def failure(identifier, code, message, **kwargs):
    return {"version": 1, "id": identifier, "error": {"code": code, "message": message, **kwargs}}


extension_dir = Path(os.environ["BLENDER_MCP_EXTENSION_DIR"])
package = types.ModuleType("bridge_test_package")
package.__path__ = [str(extension_dir)]
sys.modules[package.__name__] = package
operations = types.ModuleType(package.__name__ + ".operations")
operations.BlenderOperations = FakeOperations
operations.OperationError = OperationError
sys.modules[operations.__name__] = operations
native = types.ModuleType(package.__name__ + ".scheme_blender_mcp_native")
native.__version__ = "0.1.0"
sys.modules[native.__name__] = native
protocol = types.ModuleType(package.__name__ + ".protocol")
for key, value in {"read_frame": read_frame, "write_frame": write_frame,
                   "success": success, "failure": failure,
                   "PROTOCOL_VERSION": 1, "ProtocolError": ValueError}.items():
    setattr(protocol, key, value)
sys.modules[protocol.__name__] = protocol
handlers = types.SimpleNamespace(load_pre=[], undo_pre=[], redo_pre=[], depsgraph_update_post=[], persistent=lambda function: function)
timer_functions = set()
timers = types.SimpleNamespace(register=lambda function, **_kwargs: timer_functions.add(function),
                               unregister=timer_functions.remove,
                               is_registered=lambda function: function in timer_functions)
sys.modules["bpy"] = types.SimpleNamespace(app=types.SimpleNamespace(version_string="test", handlers=handlers, timers=timers))
spec = importlib.util.spec_from_file_location(package.__name__ + ".bridge", extension_dir / "bridge.py")
bridge = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = bridge
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.server = bridge.BridgeServer("127.0.0.1", 0)
        self.server.start()
        self.sequence = 0

    def tearDown(self):
        self.server.operations.release.set()
        self.server.stop()

    def request(self, operation, **kwargs):
        self.sequence += 1
        return {"version": 1, "id": self.sequence, "session_id": "client", "operation": operation, **kwargs}

    def rpc(self, request):
        with socket.create_connection(self.server.address, timeout=2) as connection:
            write_frame(connection, request)
            return read_frame(connection)

    def test_control_does_not_queue_or_touch_native(self):
        response = self.rpc(self.request("control_status"))["result"]
        self.assertEqual(response["state"], "ready")
        self.assertEqual(response["native_version"], "0.1.0")
        self.assertEqual(self.server.operations.executed, [])
        self.assertEqual(self.server.work.qsize(), 0)

    def test_expired_request_never_executes(self):
        item = self.server._enqueue(self.request("rna_set", deadline_unix_ms=1))
        self.server.dispatch_once()
        self.assertEqual(item.state, "expired")
        self.assertEqual(self.server.operations.executed, [])

    def test_cancel_is_scoped_to_session(self):
        request = self.request("rna_set")
        item = self.server._enqueue(request)
        other = self.rpc(self.request("cancel", request_id=request["id"], session_id="other"))
        self.assertEqual(other["error"]["code"], "receipt_unavailable")
        response = self.rpc(self.request("cancel", request_id=request["id"]))
        self.assertEqual(response["result"]["state"], "cancelled")
        self.server.dispatch_once()
        self.assertTrue(item.completed.is_set())
        self.assertEqual(self.server.operations.executed, [])

    def test_controls_work_during_running_operation(self):
        item = self.server._enqueue(self.request("rna_set", blocking=True))
        dispatch = threading.Thread(target=self.server.dispatch_once)
        dispatch.start()
        self.assertTrue(self.server.operations.entered.wait(1))
        status = self.rpc(self.request("control_status"))["result"]
        self.assertEqual(status["state"], "busy")
        cancelled = self.rpc(self.request("cancel", request_id=item.request["id"]))["result"]
        self.assertTrue(cancelled["potentially_continuing"])
        self.assertEqual(cancelled["state"], "running")
        self.server.operations.release.set()
        dispatch.join(1)
        self.assertFalse(dispatch.is_alive())
        self.assertEqual(item.state, "succeeded")
        receipt = self.rpc(self.request("request_result", request_id=item.request["id"]))["result"]
        self.assertTrue(receipt["response"]["result"]["ok"])

    def test_jobs_acknowledge_then_return_render_result(self):
        response = self.rpc(self.request("render_start", filepath="frame.png"))["result"]
        job = response["job_id"]
        self.assertEqual(response["state"], "queued")
        self.assertEqual(self.rpc(self.request("job_result", job_id=job))["error"]["code"], "result_pending")
        self.server.dispatch_once()
        self.assertEqual(self.rpc(self.request("job_status", job_id=job))["result"]["state"], "succeeded")
        result = self.rpc(self.request("job_result", job_id=job))["result"]
        self.assertEqual(result["artifact"]["id"], "frame")

    def test_render_job_rejects_conflicting_work_and_keeps_controls_available(self):
        request = self.request("render_start", blocking=True)
        job = self.rpc(request)["result"]["job_id"]
        queued = self.rpc(self.request("rna_set"))
        self.assertEqual(queued["error"]["code"], "bridge_busy")
        self.assertTrue(queued["error"]["retryable"])
        with self.assertRaisesRegex(ValueError, "duplicate request"):
            self.server._enqueue(request)
        dispatch = threading.Thread(target=self.server.dispatch_once)
        dispatch.start()
        try:
            self.assertTrue(self.server.operations.entered.wait(1))
            for operation in ("rna_set", "render_start"):
                busy = self.rpc(self.request(operation))
                self.assertEqual(busy["error"]["code"], "bridge_busy")
                self.assertTrue(busy["error"]["retryable"])
            self.assertEqual(self.rpc(self.request("job_status", job_id=job))["result"]["state"], "running")
            self.assertEqual(self.rpc(self.request("control_status"))["result"]["state"], "busy")
            self.assertTrue(self.rpc(self.request("job_cancel", job_id=job))["result"]["cancel_requested"])
        finally:
            self.server.operations.release.set()
            dispatch.join(1)
        self.assertFalse(dispatch.is_alive())
        later = self.server._enqueue(self.request("rna_set"))
        self.server.dispatch_once()
        self.assertEqual(later.state, "succeeded")

    def test_generation_and_instance_guard_mutations(self):
        mismatch = self.rpc(self.request("rna_set", expected_instance="old"))
        self.assertEqual(mismatch["error"]["code"], "instance_changed")
        item = self.server._enqueue(self.request("rna_set", expected_generation=1))
        self.server.invalidate()
        self.server.dispatch_once()
        self.assertEqual(item.state, "failed")
        self.assertEqual(self.server.operations.executed, [])

    def test_batch_yields_preserving_order_and_partial_cancel(self):
        item = self.server._enqueue(self.request("batch", requests=[{"operation": "rna_set", "delay": 0.025}] * 3))
        later = self.server._enqueue(self.request("rna_set"))
        self.server.dispatch_once()
        self.assertEqual(item.batch_index, 1)
        self.assertEqual(later.state, "queued")
        self.rpc(self.request("cancel", request_id=item.request["id"]))
        self.server.dispatch_once()
        self.assertEqual(item.state, "cancelled")
        self.assertEqual(item.cached_response["error"]["data"]["completed"], 1)
        self.server.dispatch_once()
        self.assertEqual(later.state, "succeeded")

    def test_batch_validation_precedes_mutations(self):
        item = self.server._enqueue(self.request("batch", requests=[{"operation": "rna_set"}, {"operation": "shutdown"}]))
        self.server.dispatch_once()
        self.assertEqual(item.state, "failed")
        self.assertEqual(self.server.operations.executed, [])

    def test_stop_completes_queued_waiters(self):
        item = self.server._enqueue(self.request("rna_set"))
        self.server.stop()
        self.assertTrue(item.completed.is_set())
        self.assertEqual(item.state, "cancelled")

    def test_stop_releases_waiter_while_native_execution_continues(self):
        request = self.request("rna_set", blocking=True)
        replies = []
        waiter = threading.Thread(target=lambda: replies.append(self.rpc(request)))
        waiter.start()
        limit = time.monotonic() + 1
        while not self.server.registry and time.monotonic() < limit:
            time.sleep(0.005)
        dispatch = threading.Thread(target=self.server.dispatch_once)
        dispatch.start()
        self.assertTrue(self.server.operations.entered.wait(1))
        self.server.stop()
        waiter.join(1)
        self.assertFalse(waiter.is_alive())
        self.assertTrue(replies[0]["error"]["potentially_continuing"])
        item = self.server.registry[("client", request["id"])]
        self.assertEqual(item.state, "running")
        self.assertFalse(item.completed.is_set())
        self.server.operations.release.set()
        dispatch.join(1)
        self.assertEqual(item.state, "succeeded")
        self.assertIsNone(item.response)

    def test_receipt_events_and_recent_requests_are_bounded_metadata(self):
        for _ in range(20):
            item = self.server._enqueue(self.request("rna_set", secret_payload="omitted"))
            self.server.dispatch_once()
        events = item.cached_response["events"]
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["session_id"], "client")
        status = self.rpc(self.request("control_status"))["result"]
        self.assertEqual(status["journal_scope"], "mcp_requests_only")
        self.assertEqual(len(status["recent_requests"]), 16)
        self.assertNotIn("secret_payload", json.dumps(events + status["recent_requests"]))

    def test_receipt_metrics_separate_queue_execution_and_batch_yields(self):
        clock = [100.0]

        def execute():
            clock[0] += 0.03

        self.server.operations.on_execute = execute
        with patch.object(bridge.time, "monotonic", side_effect=lambda: clock[0]):
            request = self.request("batch", requests=[{"operation": "rna_set"}] * 2)
            item = self.server._enqueue(request)
            item.submitted = clock[0]
            admitted_bytes = item.request_size
            clock[0] = 104.0
            self.server.dispatch_once()
            clock[0] = 107.0
            paused = self.server._snapshot(item)
            self.assertAlmostEqual(paused["queue_wait_seconds"], 4.0)
            self.assertAlmostEqual(paused["execution_seconds"], 0.03)
            self.server.dispatch_once()
            clock[0] = 109.0
            self.server.dispatch_once()
            receipt = self.server._snapshot(item)
            self.assertEqual(receipt["state"], "succeeded")
            self.assertAlmostEqual(receipt["queue_wait_seconds"], 4.0)
            self.assertAlmostEqual(receipt["execution_seconds"], 0.06)
            self.assertAlmostEqual(receipt["elapsed_seconds"], 5.0)
            self.assertEqual(receipt["admitted_request_bytes"], admitted_bytes)
            self.assertEqual(item.request_size, 0)
            response_bytes = len(json.dumps(item.cached_response).encode())
            self.assertEqual(receipt["response_bytes"], response_bytes)
            self.assertEqual(receipt["cached_response_bytes"], response_bytes)
            self.assertGreaterEqual(receipt["cache_serialization_seconds"], 0)
            event = item.cached_response["events"][0]
            for key in ("queue_wait_seconds", "execution_seconds", "admitted_request_bytes"):
                self.assertEqual(event[key], receipt[key])
            self.assertAlmostEqual(self.server.execution_seconds, receipt["execution_seconds"])
            with patch.object(bridge, "CACHE_BYTES", 0):
                self.server._prune()
            evicted = self.server._snapshot(item)
            self.assertEqual(evicted["cached_response_bytes"], 0)
            self.assertEqual(evicted["response_bytes"], response_bytes)

    def test_controls_advertise_limits_used_by_bridge_admission_and_cache(self):
        status = self.rpc(self.request("control_status"))["result"]
        self.assertEqual(status["limits"], {
            "work_queue": self.server.work.maxsize,
            "connections": bridge.CONNECTION_LIMIT,
            "synchronous_waiters": bridge.WAITER_LIMIT,
            "request_registry": bridge.REGISTRY_LIMIT,
            "request_bytes": bridge.REQUEST_BYTES,
            "pending_request_bytes": bridge.PENDING_REQUEST_BYTES,
            "result_cache_bytes": bridge.CACHE_BYTES,
            "cached_response_bytes": bridge.RESULT_BYTES,
            "batch_slice_ms": bridge.BATCH_SLICE_MS,
            "batch_result_bytes": bridge.RESULT_BYTES,
        })
        self.assertEqual(status["byte_accounting"], "bridge_json_utf8")
        with patch.object(bridge, "REQUEST_BYTES", 64):
            changed = self.rpc(self.request("control_status"))["result"]
            self.assertEqual(changed["limits"]["request_bytes"], 64)
            with self.assertRaises(bridge.queue.Full):
                self.server._enqueue(self.request("rna_set", value="large" * 100))

    def test_bind_failure_is_synchronous(self):
        other = bridge.BridgeServer("127.0.0.1", self.server.port)
        with self.assertRaises(OSError):
            other.start()
        self.assertIsNone(other.thread)
        self.assertIsNone(other.listener)

    def test_registry_and_results_are_bounded(self):
        for _ in range(bridge.REGISTRY_LIMIT + 5):
            item = self.server._enqueue(self.request("rna_get"))
            self.server.dispatch_once()
        self.assertLessEqual(len(self.server.registry), bridge.REGISTRY_LIMIT)
        self.assertEqual(item.request_size, 0)
        item.cached_response = None
        response = self.rpc(self.request("request_result", request_id=item.request["id"]))
        self.assertEqual(response["error"]["code"], "result_evicted")

    def test_duplicate_request_never_executes_twice(self):
        request = self.request("rna_set")
        self.server._enqueue(request)
        with self.assertRaises(ValueError):
            self.server._enqueue(request)
        self.server.dispatch_once()
        self.assertEqual(len(self.server.operations.executed), 1)

    def test_cancel_overtaking_original_request_prevents_execution(self):
        request = self.request("rna_set")
        self.rpc(self.request("cancel", request_id=request["id"]))
        item = self.server._enqueue(request)
        self.assertEqual(item.state, "cancelled")
        self.assertFalse(self.server.dispatch_once())
        self.assertEqual(self.server.operations.executed, [])

    def test_document_change_stops_batch_between_commands(self):
        item = self.server._enqueue(self.request("batch", requests=[{"operation": "rna_set", "delay": 0.025}] * 3))
        self.server.dispatch_once()
        self.server.invalidate()
        self.server.dispatch_once()
        self.assertEqual(item.state, "failed")
        self.assertEqual(item.cached_response["error"]["code"], "stale_reference")
        self.assertEqual(len(self.server.operations.executed), 1)

    def test_document_change_inside_command_stops_the_same_batch_slice(self):
        self.server.operations.on_execute = self.server.invalidate
        item = self.server._enqueue(self.request("batch", requests=[{"operation": "operator_call"}, {"operation": "rna_set"}]))
        self.server.dispatch_once()
        self.assertEqual(item.state, "failed")
        self.assertEqual(item.cached_response["error"]["code"], "stale_reference")
        self.assertEqual(item.cached_response["error"]["data"]["completed"], 1)
        self.assertEqual(len(self.server.operations.executed), 1)
        self.assertEqual(self.server.generation, 2)
        self.assertFalse(self.server.executing_native)

    def test_document_change_is_applied_when_command_raises(self):
        self.server.operations.on_execute = self.server.invalidate
        item = self.server._enqueue(self.request("batch", requests=[{"operation": "fail"}, {"operation": "rna_set"}]))
        self.server.dispatch_once()
        self.assertEqual(item.state, "failed")
        self.assertEqual(self.server.generation, 2)
        self.assertFalse(self.server.invalidation_pending)
        self.assertFalse(self.server.executing_native)
        self.assertEqual(len(self.server.operations.executed), 1)

    def test_load_epoch_is_published_before_completion(self):
        self.server.operations.on_execute = self.server.invalidate
        item = self.server._enqueue(self.request("operator_call"))
        self.server.dispatch_once()
        self.assertEqual(item.state, "succeeded")
        status = self.rpc(self.request("control_status"))["result"]
        self.assertEqual(status["generation"], 2)

    def test_timer_serves_a_sequential_stream_without_waiting_an_interval(self):
        # A sequential client sends its next request just after the previous reply.
        # The tick used to return and come back 20 ms later, so every bridge call cost
        # a whole interval; now the follow-up is served within the same tick.
        started = time.monotonic()
        self.assertEqual(self.server.timer_tick(), bridge.IDLE_INTERVAL)
        self.assertLess(time.monotonic() - started, bridge.NEXT_REQUEST_WAIT, "an idle tick must not block")

        follow_up = threading.Event()

        def enqueue_next():
            if not follow_up.is_set():
                follow_up.set()
                threading.Timer(0.0005, lambda: self.server._enqueue(self.request("rna_get"))).start()

        self.server.operations.on_execute = enqueue_next
        self.server._enqueue(self.request("rna_get"))
        self.assertEqual(self.server.timer_tick(), bridge.BUSY_INTERVAL)
        self.assertEqual(len(self.server.operations.executed), 2)

        # Once the stream stops, the timer falls back to its idle interval.
        self.server.last_work = time.monotonic() - bridge.BUSY_LINGER
        self.assertEqual(self.server.timer_tick(), bridge.IDLE_INTERVAL)

    def test_live_stop_unregisters_timer_and_hooks(self):
        server = bridge.start_live(0)
        self.assertTrue(timers.is_registered(server.timer_tick))
        self.assertIn(bridge._lifecycle, handlers.load_pre)
        bridge.stop_live()
        self.assertFalse(timers.is_registered(server.timer_tick))
        self.assertNotIn(bridge._lifecycle, handlers.load_pre)
        self.assertTrue(server.operations.closed)
        self.assertIsNone(bridge.get_server())

    def test_external_subdata_changes_defer_invalidation(self):
        server = bridge.start_live(0)
        depsgraph = types.SimpleNamespace(updates=[types.SimpleNamespace(is_updated_geometry=True, is_updated_shading=False)])
        try:
            server.executing_native = True
            bridge._depsgraph(None, depsgraph)
            self.assertFalse(server.subdata_dirty)
            server.executing_native = False
            bridge._depsgraph(None, depsgraph)
            self.assertTrue(server.subdata_dirty)
            self.assertEqual(server.operations.subdata_invalidations, 0)
            server.dispatch_once()
            self.assertEqual(server.operations.subdata_invalidations, 1)
            self.assertEqual(server.generation, 1)
        finally:
            bridge.stop_live()

    def test_headless_registers_persistent_lifecycle_hooks(self):
        original = bridge.BridgeServer.run_headless
        observed = []

        def run(server):
            self.assertIs(bridge.get_server(), server)
            self.assertIn(bridge._lifecycle, handlers.load_pre)
            bridge._lifecycle(None)
            server._apply_invalidation()
            observed.append(server.generation)
            server.operations.close()

        bridge.BridgeServer.run_headless = run
        try:
            bridge.run_headless("127.0.0.1", 0)
        finally:
            bridge.BridgeServer.run_headless = original
        self.assertEqual(observed, [2])
        self.assertIsNone(bridge.get_server())
        self.assertNotIn(bridge._lifecycle, handlers.load_pre)


if __name__ == "__main__":
    unittest.main()
