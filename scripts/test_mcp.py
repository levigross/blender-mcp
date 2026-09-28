"""Exercise the packaged MCP server against its pinned background Blender.

Run through `nix build .#checks.x86_64-linux.mcp-headless`. The standalone entry
point accepts a packaged blender-mcp binary; no external Python packages are used.
"""

import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
import urllib.request


PROTOCOL_VERSION = "2026-07-28"


class McpClient:
    def __init__(self, address, *, tasks=False):
        self.address = address
        self.next_id = 0
        self.calls = 0
        self.tasks = tasks

    def rpc(self, method, params, name=None, expect_error=False, expect_rpc_error=False):
        self.next_id += 1
        self.calls += 1
        params = dict(params)
        params["_meta"] = {
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": (
                {"extensions": {"io.modelcontextprotocol/tasks": {}}} if self.tasks else {}
            ),
        }
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": PROTOCOL_VERSION,
            "Mcp-Method": method,
        }
        if name:
            headers["Mcp-Name"] = name
        request = urllib.request.Request(
            f"http://{self.address}/mcp",
            headers=headers,
            data=json.dumps({
                "jsonrpc": "2.0", "id": self.next_id,
                "method": method, "params": params,
            }).encode(),
        )
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                raw = response.read().decode()
        except urllib.error.HTTPError as error:
            detail = error.read(4096).decode(errors="replace")
            if not expect_rpc_error or error.code != 400:
                raise AssertionError(f"{method}: HTTP {error.code}: {detail}") from error
            raw = detail
        if raw.lstrip().startswith("{"):
            reply = json.loads(raw)
        else:
            events = (
                json.loads("\n".join(
                    line.removeprefix("data:").lstrip()
                    for line in event.splitlines() if line.startswith("data:")
                ))
                for event in raw.replace("\r\n", "\n").split("\n\n")
                if any(line.startswith("data:") for line in event.splitlines())
            )
            reply = next(event for event in events if event.get("id") == self.next_id)
        if expect_rpc_error:
            assert "error" in reply, reply
            return reply["error"]
        assert "error" not in reply, reply
        result = reply["result"]
        assert bool(result.get("isError")) == expect_error, result
        return result

    def evaluate(self, code, expect_error=False, *, session=None, reset=False):
        arguments = {"code": code}
        if session is not None:
            arguments["session"] = session
        if reset:
            arguments["reset"] = True
        result = self.rpc(
            "tools/call", {"name": "scheme_eval", "arguments": arguments},
            "scheme_eval", expect_error=expect_error,
        )
        if not expect_error:
            assert result["structuredContent"]["result_complete"], result
        return result

    def value(self, code, *, session=None):
        return self.evaluate(code, session=session)["structuredContent"]["result"]


class Server:
    """An isolated, owned process tree; also reused by the Nix benchmark."""

    def __init__(self, binary, *, sessions=None):
        self.binary = str(Path(binary).resolve())
        self.workspace = tempfile.TemporaryDirectory(prefix="blender-mcp-test-")
        self.root = Path(self.workspace.name)
        self.log = (self.root / "server.log").open("w+")
        self.process = None
        self.sessions = sessions

    def start(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            address = f"127.0.0.1:{listener.getsockname()[1]}"
        environment = dict(os.environ)
        for name in [
            "BLENDER_MCP_TOKEN", "BLENDER_MCP_BLENDER", "BLENDER_MCP_BOOTSTRAP",
            "BLENDER_MCP_EXTENSION_DIR", "BLENDER_MCP_INSTANCE_ID", "BLENDER_MCP_SESSIONS_FILE",
        ]:
            environment.pop(name, None)
        environment.update({
            "BLENDER_MCP_GPU": "0", "ALSOFT_DRIVERS": "null",
            "BLENDER_USER_CONFIG": str(self.root / "config"),
            "BLENDER_USER_SCRIPTS": str(self.root / "scripts"),
            "BLENDER_USER_EXTENSIONS": str(self.root / "extensions"),
        })
        command = [self.binary, "--backend", "headless", "--bind", address,
                   "--startup-timeout-secs", "90"]
        if self.sessions is not None:
            sessions_file = self.root / "sessions.json"
            sessions_file.write_text(json.dumps(self.sessions))
            command.extend(["--sessions-file", str(sessions_file)])
        self.process = subprocess.Popen(
            command,
            env=environment, stdout=self.log, stderr=self.log,
            start_new_session=True,
        )
        return address

    def __enter__(self):
        address = self.start()
        try:
            deadline = time.monotonic() + 100 * (1 + len(self.sessions or []))
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise RuntimeError("packaged server exited during startup")
                try:
                    with urllib.request.urlopen(f"http://{address}/healthz", timeout=1) as response:
                        self.health = json.load(response)
                    self.client = McpClient(address)
                    return self
                except (OSError, ValueError):
                    time.sleep(0.1)
            raise TimeoutError("packaged server did not become ready")
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def __exit__(self, error_type, error, traceback):
        shutdown_error = None
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
                shutdown_error = "packaged server did not stop within 15 seconds"
        if self.process is not None and self.process.returncode != 0:
            shutdown_error = shutdown_error or f"packaged server exited with {self.process.returncode}"
        if error is not None or shutdown_error is not None:
            self.log.seek(0)
            print(self.log.read()[-12000:], file=sys.stderr)
        self.log.close()
        self.workspace.cleanup()
        if error is None and shutdown_error is not None:
            raise RuntimeError(shutdown_error)


def basic_scene(client):
    assert [tool["name"] for tool in client.rpc("tools/list", {})["tools"]] == ["scheme_eval"]
    client.evaluate('''
        (define cube (add-cube))
        (set-rotation! cube 0 90 180)
        (rna-call cube "keyframe_insert" (list "location") (hash "frame" 1))
        (set-location! cube 0 0 3)
        (rna-call cube "keyframe_insert" (list "location") (hash "frame" 11))
        (rna-call (scene) "frame_set" (list 6))
    ''')
    # A new HTTP client observes the same Scheme binding and evaluated Blender pose.
    reconnected = McpClient(client.address)
    pose = reconnected.value("(object-location cube)")
    assert 0 < pose[2] < 3, pose
    assert client.value('''(length (collection-list
        (rna-get (car (collection-list (bpy-data "screens"))) "areas")))''') > 0
    client.evaluate('''
        (define ss-modtest (add-modifier! cube "Tailored surface" "SUBSURF"))
        (rna-set! ss-modtest "levels" 1)
    ''')
    assert client.value('(rna-get ss-modtest "levels")') == 1
    client.evaluate('''
        (op-call "object.shade_smooth")
        (set-engine! "CYCLES") (set-resolution! 16 16) (set-samples! 1)
        (rna-set! (cycles-settings) "use_denoising" #f)
        (rna-set! (cycles-settings) "device" "CPU")
    ''')
    assert client.value('(rna-get ss-modtest "levels")') == 1


def build_scene(client, root):
    """The stdlib build helpers against real headless Blender, end to end."""
    client.evaluate('''
        (define build (make-collection! "Build"))
        (define hex (make-prism "Hex" 6 1.0 0.5 build))
        (define hex-bevel (bevel! hex 0.02 2))
        (subdivide! hex 1)
        (define block (add-cube))
        (set-location! block 3 0 0.5)
        (move-to-collection! block build)
        (define pivot (add-empty "PLAIN_AXES" (list 1 1 0)))
        (parent! block pivot)
        (turntable! pivot 1 5)
        (prop-set! hex "role" "walkable_ground")
        (define rig (three-point-lights! (list 0 0 0) 5.0))
        (define shot (add-camera (list 0 -10 3)))
        (set-camera! shot)
        (frame-camera! shot (list hex block))
    ''')
    polygons = client.value('(length (collection-list (rna-get (rna-get hex "data") "polygons")))')
    assert polygons == 8, polygons
    assert client.value('(map (lambda (c) (rna-get c "name")) (object-collections hex))') == ["Build"]
    assert client.value('(map (lambda (c) (rna-get c "name")) (object-collections block))') == ["Build"]
    assert client.value('(prop-get hex "role")') == "walkable_ground"
    assert client.value('(prop-keys hex)') == ["role"]
    assert client.value('(length rig)') == 3
    assert client.value('(map (lambda (m) (rna-get m "name")) (collection-list (rna-get hex "modifiers")))') == [
        "Bevel", "Subdivision"]

    # parent! must not move the child; at frame 1 the pivot has not turned yet.
    client.evaluate('(set-frame! 1)')
    world = client.value('(begin (refresh!) (list-ref (rna-get block "matrix_world") 0))')
    assert abs(world[3] - 3.0) < 1e-5, world
    # Half way through a linear turn the pivot faces the other way.
    client.evaluate('(set-frame! 3)')
    z = client.value('(list-ref (object-rotation pivot) 2)')
    assert abs(z - 180.0) < 1e-3, z

    # The framed camera looks at the centre of what it framed -- at frame 1, before
    # the turntable carried the block round.
    client.evaluate('(set-frame! 1)')
    client.evaluate('''(define framing-dot
        (let* ([bounds (scene-bounds (list hex block))]
               [centre (map (lambda (a b) (/ (+ a b) 2.0)) (car bounds) (list-ref bounds 1))]
               [aim (normalize (map - centre (object-location shot)))])
          (apply + (map * (facing shot) aim))))''')
    # Bind first: Steel lifts the inner lambda into a hidden top-level define,
    # which would otherwise add an extra value to the result.
    dot = client.value("framing-dot")
    assert dot > 0.999, dot

    # Edit mode round trip, headless: inset turns each of 6 faces into 5.
    client.evaluate('(define inset (add-cube)) (inset-faces! inset 0.1 0.05)')
    faces = client.value('''
        (length (collection-list (rna-get (rna-get inset "data") "polygons")))''')
    assert faces == 30, faces
    assert client.value('(rna-get inset "mode")') == "OBJECT"

    frame = root / "build.png"
    client.evaluate('(render-preset! "draft") (set-resolution! 16 16) (rna-set! (cycles-settings) "samples" 1)')
    written = client.value(f"(render-file! {json.dumps(str(frame))})")
    assert Path(written).exists(), written


def render_artifacts(client, root):
    output = root / "preview.png"
    quiet = client.evaluate(f"(preview! {json.dumps(str(output))} 50)")
    assert quiet["structuredContent"]["result"] == str(output), quiet
    assert quiet["structuredContent"]["artifacts"] == []
    assert len(quiet["content"]) == 1
    assert output.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    assert client.value('(rna-get (render-settings) "resolution_percentage")') == 100
    inline = client.evaluate("(render!)")
    images = [block for block in inline["content"] if block["type"] == "image"]
    assert len(images) == 1, inline
    image = base64.b64decode(images[0]["data"], validate=True)
    assert image.startswith(b"\x89PNG\r\n\x1a\n")
    assert "data_base64" not in json.dumps(inline["structuredContent"])
    return inline["structuredContent"]["artifacts"][0], image


def runtime_and_scene(client, binary):
    status = client.value("(control-status)")
    assert status["instance_id"] and status["protocol_version"] == 1, status
    assert status["extension_version"] and status["native_version"], status
    bridge_source = Path(binary).parent.parent / "share/blender-mcp/scheme_blender_mcp/bridge.py"
    assert status["build_id"] == hashlib.sha256(bridge_source.read_bytes()).hexdigest(), status
    assert {"control_status", "render_jobs", "reference_epochs", "batch"} <= set(status["capabilities"])
    assert status["state"] == "ready", status
    property_info = client.value('(rna-property-info cube "location")')
    function_info = client.value('(rna-function-info cube "keyframe_insert")')
    assert property_info["identifier"] == "location", property_info
    assert function_info["identifier"] == "keyframe_insert", function_info
    client.evaluate('''
        (define before-change (scene-snapshot))
        (define cube-location (rna-get-command cube "location"))
    ''')
    batch = client.value('''(batch! (list
        (hash "operation" "rna_set" "reference" cube
              "attribute" "location" "value" (list 1 2 3)) cube-location))''')
    assert batch["complete"] and batch["completed"] == 2, batch
    assert batch["results"][1]["result"] == [1, 2, 3], batch
    difference = client.value("(scene-diff before-change)")
    assert not difference["unchanged"] and len(difference["changed"]) == 1, difference
    assert difference["added"] == difference["removed"] == [], difference
    client.evaluate("(define unchanged-scene (scene-snapshot))")
    assert client.value("(scene-diff unchanged-scene)")["unchanged"]
    # Validation of an oversized batch must happen before its first mutation.
    too_many = " ".join(["cube-location"] * 100)
    client.evaluate(f'''(batch! (list
        (hash "operation" "rna_set" "reference" cube
              "attribute" "location" "value" (list 8 8 8)) {too_many}))''', expect_error=True)
    assert client.value("(object-location cube)") == [1, 2, 3]
    partial = client.value('''(batch! (list
        cube-location
        (hash "operation" "rna_get" "reference" cube "attribute" "_private")
        (hash "operation" "rna_set" "reference" cube
              "attribute" "location" "value" (list 9 9 9))))''')
    assert not partial["complete"] and partial["completed"] == 1, partial
    assert partial["failed_index"] == 1, partial
    assert client.value("(object-location cube)") == [1, 2, 3]


def immutable_artifacts(client, artifact, original_bytes):
    client.evaluate("(set-resolution! 32 32)")
    later = client.evaluate("(render!)")["structuredContent"]["artifacts"][0]
    assert later["id"] != artifact["id"], (artifact, later)
    assert (artifact["width"], artifact["height"]) == (16, 16), artifact
    assert (later["width"], later["height"]) == (32, 32), later
    metadata = client.evaluate(f'(artifact-info "{artifact["id"]}")')
    assert len(metadata["content"]) == 1 and metadata["structuredContent"]["artifacts"] == [], metadata
    assert "data_base64" not in json.dumps(metadata), metadata
    assert metadata["structuredContent"]["result"]["sha256"] == artifact["sha256"]
    fetched = client.evaluate(f'(artifact-get "{artifact["id"]}")')
    image = next(block for block in fetched["content"] if block["type"] == "image")
    assert base64.b64decode(image["data"], validate=True) == original_bytes
    uri = f'blender-mcp://artifact/{artifact["id"]}'
    resource = client.rpc("resources/read", {"uri": uri}, uri)["contents"][0]
    assert resource["mimeType"] == "image/png", resource
    assert base64.b64decode(resource["blob"], validate=True) == original_bytes
    released = client.value(f'(artifact-release! "{artifact["id"]}")')
    assert released["released"] == 1, released
    client.evaluate(f'(artifact-info "{artifact["id"]}")', expect_error=True)


def thumbnail(client):
    client.evaluate('''
        (set-resolution! 32 32)
        (define image-settings (rna-get (render-settings) "image_settings"))
        (rna-set! image-settings "file_format" "JPEG")
    ''')
    result = client.evaluate('(thumbnail! (hash "max_size" 12))')
    artifact = result["structuredContent"]["artifacts"][0]
    assert artifact["mime_type"] == "image/png", artifact
    assert 0 < artifact["width"] <= 12 and 0 < artifact["height"] <= 12, artifact
    assert client.value('''(list
        (rna-get (render-settings) "resolution_x")
        (rna-get (render-settings) "resolution_y")
        (rna-get (render-settings) "resolution_percentage")
        (rna-get image-settings "file_format"))''') == [32, 32, 100, "JPEG"]
    client.evaluate('(rna-set! image-settings "file_format" "PNG")')


def render_job(client):
    job = client.value("(render-start)")
    identifier = job["job_id"]
    # Reconnect using a new HTTP client before inspecting or retrieving the job.
    reconnected = McpClient(client.address)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        status = reconnected.value(f'(job-status "{identifier}")')
        if status["state"] in {"succeeded", "failed", "cancelled", "expired"}:
            break
        control = reconnected.value("(control-status)")
        assert control["state"] in {"ready", "busy"}, control
        time.sleep(0.05)
    else:
        raise TimeoutError(f"render job did not finish: {status}")
    assert status["state"] == "succeeded", status
    metadata = reconnected.evaluate(f'(job-result "{identifier}" #f)')
    assert len(metadata["content"]) == 1, metadata
    completed = reconnected.evaluate(f'(job-result "{identifier}")')
    assert any(block["type"] == "image" for block in completed["content"]), completed


def references_and_checkpoint(client, root):
    original_path = client.value('(rna-get (data-ref) "filepath")')
    original_location = client.value("(object-location cube)")
    generation = client.value("(control-status)")["generation"]
    checkpoint = root / "checkpoint.blend"
    client.evaluate(f"(checkpoint! {json.dumps(str(checkpoint))})")
    assert checkpoint.stat().st_size > 0
    assert client.value('(rna-get (data-ref) "filepath")') == original_path
    assert client.value("(control-status)")["generation"] == generation
    assert client.value("(object-location cube)") == original_location
    client.evaluate("(define released-root (data-ref))")
    released = client.value("(reference-release! (list released-root))")
    assert released["released"] == 1, released
    stale = client.evaluate('(rna-get released-root "objects")', expect_error=True)
    assert "stale_reference" in json.dumps(stale), stale
    client.evaluate(f'''(op-call "wm.open_mainfile"
        (hash "filepath" {json.dumps(str(checkpoint))}))''')
    stale = client.evaluate("(object-location cube)", expect_error=True)
    assert "stale_reference" in json.dumps(stale), stale
    assert client.value("(control-status)")["generation"] != generation
    assert client.value("(scene-summary)")["objects"] > 0


def output_budget(client):
    incomplete = client.rpc(
        "tools/call",
        {"name": "scheme_eval", "arguments": {
            "code": "(define output-budget-marker 37) (range 0 11000)",
        }},
        "scheme_eval",
    )["structuredContent"]
    assert not incomplete["result_complete"] and incomplete["serialization_error"], incomplete
    assert client.value("output-budget-marker") == 37


def resources_and_tasks(client):
    index = client.rpc("resources/read", {"uri": "resources://blender"}, "resources://blender")
    assert "resources://blender/guide/tasks" in index["contents"][0]["text"], index
    templates = client.rpc("resources/templates/list", {})["resourceTemplates"]
    assert any(item["uriTemplate"] == "resources://blender/artifact/{id}" for item in templates), templates
    capable = McpClient(client.address, tasks=True)
    created = capable.rpc("tools/call", {"name": "scheme_eval", "arguments": {
        "code": '(begin (rna-get cube "name") 42)', "background": True,
    }}, "scheme_eval")
    assert created["resultType"] == "task", created
    reconnected = McpClient(client.address, tasks=True)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        task = reconnected.rpc("tasks/get", {"taskId": created["taskId"]}, created["taskId"])
        if task["status"] == "completed":
            assert task["result"]["structuredContent"]["result"] == 42, task
            assert not task["result"].get("isError"), task
            break
        assert task["status"] == "working", task
        time.sleep(0.02)
    else:
        raise AssertionError("background task did not finish")


def multiplex(server):
    client = server.client
    other_client = McpClient(client.address)
    assert [tool["name"] for tool in client.rpc("tools/list", {})["tools"]] == ["scheme_eval"]
    uri = "resources://blender/sessions"
    resource = client.rpc("resources/read", {"uri": uri}, uri)["contents"][0]
    sessions = json.loads(resource["text"])
    assert sessions["default_session"] == "default", sessions
    assert {item["name"] for item in sessions["sessions"]} == {"default", "other"}, sessions
    for session in ["default", "other"]:
        for suffix in ["status", "catalog"]:
            uri = f"resources://blender/sessions/{session}/runtime/{suffix}"
            resource = client.rpc("resources/read", {"uri": uri}, uri)["contents"][0]
            assert resource["uri"] == uri and resource["text"], resource

    default_status = client.value("(control-status)")
    other_status = client.value("(control-status)", session="other")
    assert default_status["instance_id"] != other_status["instance_id"], (default_status, other_status)
    for session, marker, name in [
        ("default", 101, "MultiplexDefault"), ("other", 202, "MultiplexOther"),
    ]:
        result = client.evaluate(f'''
            (define multiplex-marker {marker})
            (define multiplex-cube (add-cube))
            (rename! multiplex-cube "{name}")
        ''', session=session)
        assert result["structuredContent"]["session"] == session, result
    # Two independent HTTP clients share one named Scheme worker and Blender scene.
    assert other_client.value("multiplex-marker", session="other") == 202
    assert other_client.value('(rna-get multiplex-cube "name")', session="other") == "MultiplexOther"
    other_client.evaluate("(set-location! multiplex-cube 1 2 3)", session="other")
    assert client.value("(object-location multiplex-cube)", session="other") == [1, 2, 3]

    def increment(peer):
        return peer.value('''(begin
            (set! multiplex-marker (+ multiplex-marker 1))
            multiplex-marker)''', session="other")

    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(increment, peer) for peer in [client, other_client]]
        assert sorted(future.result() for future in futures) == [203, 204]
    assert client.value("multiplex-marker") == 101
    default_objects = client.value("(object-names)")
    other_objects = client.value("(object-names)", session="other")
    assert "MultiplexDefault" in default_objects and "MultiplexOther" not in default_objects, default_objects
    assert "MultiplexOther" in other_objects and "MultiplexDefault" not in other_objects, other_objects
    client.evaluate("(define other-only 909)", session="other")
    client.evaluate("other-only", expect_error=True)
    client.evaluate("(+ 1 2)", session="other", reset=True)
    other_client.evaluate("multiplex-marker", expect_error=True, session="other")
    assert client.value("multiplex-marker") == 101
    assert "MultiplexOther" in client.value("(object-names)", session="other")

    # A background render keeps its selected Blender and artifact store on reconnect.
    other_client.evaluate('''
        (define background-marker 717)
        (set-engine! "CYCLES") (set-resolution! 16 16) (set-samples! 1)
        (rna-set! (cycles-settings) "use_denoising" #f)
        (rna-set! (cycles-settings) "device" "CPU")
    ''', session="other")
    capable = McpClient(client.address, tasks=True)
    created = capable.rpc("tools/call", {"name": "scheme_eval", "arguments": {
        "code": "(begin (render!) background-marker)", "session": "other", "background": True,
    }}, "scheme_eval")
    assert created["resultType"] == "task", created
    reconnected = McpClient(client.address, tasks=True)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        task = reconnected.rpc("tasks/get", {"taskId": created["taskId"]}, created["taskId"])
        if task["status"] == "completed":
            break
        assert task["status"] == "working", task
        time.sleep(0.05)
    else:
        raise TimeoutError("multiplex background render did not finish")
    result = task["result"]
    assert not result.get("isError"), result
    structured = result["structuredContent"]
    assert structured["session"] == "other" and structured["result"] == 717, structured
    artifact = structured["artifacts"][0]
    uri = f'resources://blender/sessions/other/artifact/{artifact["id"]}'
    assert artifact["uri"] == uri, artifact
    image = next(block for block in result["content"] if block["type"] == "image")
    image_bytes = base64.b64decode(image["data"], validate=True)
    assert image_bytes.startswith(b"\x89PNG\r\n\x1a\n"), image
    resource = reconnected.rpc("resources/read", {"uri": uri}, uri)["contents"][0]
    assert resource["uri"] == uri and resource["mimeType"] == "image/png", resource
    assert base64.b64decode(resource["blob"], validate=True) == image_bytes
    for prefix in ["resources://blender/sessions/default/artifact/", "blender-mcp://artifact/"]:
        wrong_uri = prefix + artifact["id"]
        error = reconnected.rpc("resources/read", {"uri": wrong_uri}, wrong_uri, expect_rpc_error=True)
        assert error["code"] == -32602 and "artifact_missing" in error["message"], error
    assert client.value("multiplex-marker") == 101
    client.evaluate("background-marker", expect_error=True)
    print("PASS: multiplexed clients, isolated Blender sessions, scoped reset, background tasks, and artifact routing")


def startup_shutdown(binary):
    # Stall the second session's discovery after the first Blender has started.
    with socket.socket() as bridge:
        bridge.bind(("127.0.0.1", 0))
        bridge.listen(1)
        bridge.settimeout(0.2)
        address = f"127.0.0.1:{bridge.getsockname()[1]}"
        server = Server(binary, sessions=[{"name": "stalled", "backend": "live", "bridge": address}])
        try:
            server.start()
            deadline = time.monotonic() + 100
            while time.monotonic() < deadline:
                assert server.process.poll() is None, "server exited before second-session discovery"
                try:
                    connection, _ = bridge.accept()
                    break
                except TimeoutError:
                    pass
            else:
                raise TimeoutError("server did not reach second-session discovery")
            with connection:
                pid = server.process.pid
                children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
                assert children, "default headless Blender was not running before interrupted startup"
                # Only the server receives SIGTERM; it must stop and reap its children.
                server.process.send_signal(signal.SIGTERM)
                assert server.process.wait(timeout=15) in {0, 1}, "server did not handle SIGTERM"
                remaining = [child for child in children if Path(f"/proc/{child}").exists()]
                assert not remaining, f"Blender children survived interrupted startup: {remaining}"
        except BaseException:
            if server.process is not None:
                try:
                    os.killpg(server.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                server.process.wait(timeout=15)
            server.log.seek(0)
            print(server.log.read()[-12000:], file=sys.stderr)
            raise
        finally:
            server.log.close()
            server.workspace.cleanup()
    print("PASS: SIGTERM during multi-session startup stops and reaps earlier Blender children")


def main():
    with Server(sys.argv[1]) as server:
        basic_scene(server.client)
        failures = []

        def check(function, *arguments):
            try:
                return function(*arguments)
            except Exception:
                failures.append(f"{function.__name__}:\n{traceback.format_exc()}")
                return None

        # Exercise independent paths even after a failure, then fail the check with
        # all diagnostics. Rendering does not depend on snapshot comparison working.
        check(runtime_and_scene, server.client, server.binary)
        check(resources_and_tasks, server.client)
        artifact = check(render_artifacts, server.client, server.root)
        if artifact is not None:
            check(immutable_artifacts, server.client, *artifact)
        check(thumbnail, server.client)
        check(render_job, server.client)
        check(references_and_checkpoint, server.client, server.root)
        check(output_budget, server.client)
        # Last: it leaves draft render settings and extra objects in the shared scene.
        check(build_scene, server.client, server.root)
        assert not failures, "\n".join(failures)
        print("PASS: packaged MCP, resources, tasks, persistence, discovery, batch, snapshots, immutable artifacts, jobs, and epochs")
    with Server(sys.argv[1], sessions=[{"name": "other", "backend": "headless"}]) as server:
        multiplex(server)
    startup_shutdown(sys.argv[1])


if __name__ == "__main__":
    main()
