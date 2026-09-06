"""Compare equivalent scalar and batched RNA reads and writes through packaged MCP.

Nix builds this benchmark in an isolated background Blender session. Timings are
observations, never a CI speed threshold; request counts describe the workload.
"""

import json
from pathlib import Path
import sys
import time

from test_mcp import Server


def benchmark(client, count, *, writes=False):
    indices = " ".join(map(str, range(count)))
    scalar = f'''(map (lambda (i) (rna-get benchmark-object "location"))
                     (list {indices}))'''
    commands = " ".join(["benchmark-command"] * 100)
    batches = " ".join([f"(batch! (list {commands}))"] * (count // 100))
    if writes:
        scalar = f'''(map (lambda (i) (rna-set! benchmark-object "location" (list i (- i) 1)))
                         (list {indices}))'''
        batches = " ".join(
            f'''(batch! (map (lambda (i)
                (rna-set-command benchmark-object "location" (list i (- i) 1)))
                (list {" ".join(map(str, range(offset, offset + 100)))})))'''
            for offset in range(0, count, 100)
        )
    client.value('(set-location! benchmark-object 1 2 3)')
    started = time.perf_counter()
    scalar_reply = client.evaluate(scalar)["structuredContent"]
    scalar_seconds = time.perf_counter() - started
    scalar_pose = client.value('(object-location benchmark-object)')
    client.value('(set-location! benchmark-object 1 2 3)')
    started = time.perf_counter()
    batch_reply = client.evaluate(f"(list {batches})")["structuredContent"]
    batch_seconds = time.perf_counter() - started
    batch_pose = client.value('(object-location benchmark-object)')
    scalar_values = scalar_reply["result"]
    batch_values = []
    for reply in batch_reply["result"]:
        assert reply["complete"] and reply["completed"] == 100, reply
        batch_values.extend(item["result"] for item in reply["results"])
    assert scalar_values == batch_values, "batch and scalar results must be equivalent"
    assert len(scalar_values) == count
    if writes:
        assert all(value == {"updated": "location"} for value in scalar_values)
        assert scalar_pose == batch_pose == [count - 1, 1 - count, 1]
    else:
        assert all(value == [1.0, 2.0, 3.0] for value in scalar_values)
        assert scalar_pose == batch_pose == [1, 2, 3]
    assert scalar_reply["metrics"]["bridge_calls"] == count, scalar_reply["metrics"]
    assert batch_reply["metrics"]["bridge_calls"] == count // 100, batch_reply["metrics"]
    return {
        "operations": count,
        "operation": "rna_set" if writes else "rna_get",
        "scalar": {**scalar_reply["metrics"], "http_requests": 1, "seconds": scalar_seconds},
        "batch": {**batch_reply["metrics"], "http_requests": 1, "seconds": batch_seconds},
        "speedup": scalar_seconds / batch_seconds,
        "equivalent_results": True,
    }


def main():
    with Server(sys.argv[1]) as server:
        server.client.evaluate('''
            (define benchmark-object (add-cube))
            (set-location! benchmark-object 1 2 3)
            (define benchmark-command
              (hash "operation" "rna_get"
                    "reference" (hash-try-get benchmark-object "$rna_ref")
                    "attribute" "location"))
        ''')
        result = {
            "workload": "Location reads and sequential location writes on one persistent Blender object",
            "samples_per_case": 1,
            "batch_size": 100,
            "request_count_scope": "Scheme bridge invocations; transport control preflights excluded",
            "runtime": server.client.value("(control-status)"),
            "cases": [benchmark(server.client, count, writes=writes)
                      for writes in [False, True] for count in [100, 1000]],
        }
        rendered = json.dumps(result, indent=2, sort_keys=True)
        print(rendered)
        if len(sys.argv) > 2:
            Path(sys.argv[2]).write_text(rendered + "\n")


if __name__ == "__main__":
    main()
