# Local vehicle scripting API

An external companion application can read the current player's script state,
change declared variables, dispatch a declared trigger, and refresh a vehicle
texture file it has replaced. For example, a control panel can update a vehicle's
display or insert an item through the vehicle's own script. The vehicle author
defines the variable and trigger names; this API contains no add-on-specific logic.

This is a small external counterpart to Lua's existing variable/string/trigger
functions. It does not expose process memory or make an unmodified OmsiHook DLL
compatible. Timetable assignment, scenery, traffic signals, passengers, physics,
audio and binary script-texture uploads are outside this API.

## Enable and discover

The bridge is disabled by default. Set `OMSI_NATIVE_BRIDGE=1` before starting the
game, for example:

```powershell
$env:OMSI_NATIVE_BRIDGE = '1'
& .\openomsi.exe --root 'C:\Path\To\OMSI 2' --launcher
```

The game binds an ephemeral TCP port on **127.0.0.1 only**. Read
`%USERPROFILE%\.openomsi\native-bridge.json` (or `$HOME/.openomsi/native-bridge.json`
when `USERPROFILE` is unset). Its fields are `protocol: 1`, `pid`, `port`, `token`,
`session_id` and `game_root`. The token authorizes local writes: do not log it or
ship a discovery file with a plugin. Unix discovery files are created with mode
0600. The process removes its own discovery file when the bridge shuts down.

Reconnect using fresh discovery after a process or map change. Obtain vehicle
identity from a fresh snapshot; a filename or vehicle name is not an identity.

## Exact API surface

All requests have `protocol: 1`, the discovery `token`, an unsigned 64-bit
`request_id`, and `op`. Each frame is a little-endian `u32` JSON length, that many
UTF-8 JSON bytes, and a little-endian `u32` **zero** binary length. JSON is limited
to 2 MiB; nonzero binary payloads and unknown top-level fields are rejected.

| Operation | Access | Additional request fields | Successful response |
| --- | --- | --- | --- |
| `snapshot` | Read | None | `snapshot` described below |
| `set_variables` | Write | `session_id`, `vehicle_id`, `generation`, `values`, `strings` | `result` acknowledgement |
| `vehicle.set_variables` | Write | `session_id`, `args: {id, generation, values, strings}` | Same operation with named arguments |
| `vehicle.trigger` | Execute | `session_id`, `args: {id, generation, name}` | Trigger dispatch acknowledgement in `result` |
| `invalidate_texture` | Write | `session_id`, `vehicle_id`, `generation`, `path` | Texture refresh acknowledgement in `result` |

Replies always carry `protocol`, the matching `request_id`, and `ok`. Success has
`snapshot` or `result`; failure has `error` and sometimes the `status` below.
Successful writes and trigger dispatches return `result: null`.

### Snapshot and identity

`snapshot` contains `session_id`, `sequence` (an increasing publication counter),
`game_root`, `map_name` (empty before loading), `paused`, `clock`, `capabilities`,
`native_operations` and `vehicle`. `clock` contains `hour`, `minute`, `second`,
`day`, `month`, `year`, `day_of_year` and `service_seconds`, taken from the game
clock. `service_seconds` can exceed 86400 during an overnight service. An absent
current player is `vehicle: null`. A vehicle contains:

| Field | Meaning |
| --- | --- |
| `id`, `generation` | Unsigned 64-bit identity values; preserve them without rounding |
| `file_name` | The loaded vehicle definition's path |
| `friendly_name` | The vehicle definition's manufacturer and type |
| `variables` | Object mapping available numeric script/model names to finite values |
| `strings` | Object mapping declared string script names to text |
| `triggers` | Array of declared trigger names |

`capabilities` advertises `variables` and `invalidate_texture`;
`native_operations` advertises the supported named operations. Check these before
using a feature rather than guessing from a release number. Named `args.id` and
`args.generation` also accept decimal strings, suitable for clients whose JSON
numbers cannot represent every 64-bit integer.

Snapshots are published from the game thread, normally every 50 ms, and may lag
between frames. Writes validate the live session and current vehicle/generation
when executed. A previously parked, replaced or unloaded vehicle is not writable
through this current-player API. Refresh identity instead of retrying stale writes.

### Variable batches and triggers

`values` and `strings` are JSON objects and may be omitted individually. A batch
must contain a change, at most 512 total names, names of at most 256 UTF-8 bytes,
and string values of at most 65536 bytes. Numeric values must be finite and fit
the simulator's `f32`. Every name must already exist. Names differing only by
ASCII case in one numeric/string object are rejected. The entire batch is
validated before any variable is changed.

`vehicle.trigger` dispatches **exactly** the declared `name` once. To release an
authored button, explicitly send its declared release trigger, such as a name
ending in `_off`. A successful reply confirms dispatch, not that the bus accepted
a ticket/card/button action. Read its authored variables to observe the result.
A script runtime error can occur after the script has changed state; it is not a
transaction that rolls back the bus script.

### Refreshing a replaced file texture

Write the new file first, then send `invalidate_texture`. `path` is relative to
the current vehicle sections' texture directories; absolute paths, `..`, and
resolved paths escaping those directories are rejected. Only an already resident
file texture is refreshed, at most 4096 by 4096 pixels. No arbitrary GPU texture
is created, and no map or vehicle definition is changed.

The decoded replacement updates the resident texture references and invalidates
the CPU cache. Refreshed paths are retained against stale streaming uploads, with
a limit of 64 resident path keys per world. This is a disk-file refresh, separate
from script-texture upload or material editing.

## Execution and retries

Socket workers only parse messages, read published snapshots and queue commands.
The game thread executes at most four queued commands per frame and publishes the
resulting snapshot before acknowledging them. The bridge is serviced even without
Lua/DLL plugins and while paused; bus frame scripts resume when simulation resumes.
Four clients and four queued commands are allowed. A busy or expired request can
be rejected without execution. Each connection allows at most 120 requests per
second and uses a two-second I/O and command acknowledgement timeout.

Use increasing request IDs on one connection. If a command times out before it
starts, it is cancelled. If it has started, `status: "indeterminate"` means it may
have changed state. Resend the **same ID on that same connection** to retrieve its
receipt without executing twice. Only the latest command receipt is retained;
older IDs are rejected. After disconnect, read current state before deciding on
a new command. Do not blindly repeat a trigger under a fresh ID.

`status: "completed_reply_too_large"` likewise does not undo an operation. Inspect
the current state. Invalid framing or authentication closes the connection.

## Minimal client (Python standard library)

This runnable example only reads the active vehicle. It uses no third-party
packages. The final commented calls show the write forms; substitute names that
the loaded vehicle actually declares.

```python
import itertools
import json
import os
from pathlib import Path
import socket
import struct

profile = Path(os.environ.get("USERPROFILE") or os.environ["HOME"])
discovery = json.loads((profile / ".openomsi/native-bridge.json").read_text())
assert discovery["protocol"] == 1
ids = itertools.count(1)

with socket.create_connection(("127.0.0.1", discovery["port"]), timeout=5) as sock:
    def receive(n):
        data = bytearray()
        while len(data) < n:
            part = sock.recv(n - len(data))
            if not part:
                raise EOFError("bridge disconnected")
            data.extend(part)
        return data

    def call(op, **fields):
        request_id = next(ids)
        request = dict(protocol=1, token=discovery["token"],
                       request_id=request_id, op=op, **fields)
        data = json.dumps(request).encode("utf-8")
        sock.sendall(struct.pack("<I", len(data)) + data + b"\0\0\0\0")
        size, = struct.unpack("<I", receive(4))
        if not 0 < size <= 2 * 1024 * 1024:
            raise ValueError("invalid response length")
        reply = json.loads(receive(size))
        assert receive(4) == b"\0\0\0\0"
        assert reply["protocol"] == 1 and reply["request_id"] == request_id
        if not reply["ok"]:
            # Stop here: this example intentionally never retries a mutation.
            raise RuntimeError(reply)
        return reply

    snapshot = call("snapshot")["snapshot"]
    vehicle = snapshot["vehicle"]
    if vehicle:
        print(vehicle["file_name"])
        print("Variables:", list(vehicle["variables"]))
        print("Triggers:", vehicle["triggers"])
        identity = dict(session_id=snapshot["session_id"],
                        vehicle_id=vehicle["id"], generation=vehicle["generation"])
        # call("set_variables", **identity, values={"display_visible": 1})
        # call("vehicle.trigger", session_id=snapshot["session_id"],
        #      args=dict(id=str(vehicle["id"]), generation=str(vehicle["generation"]),
        #                name="display_confirm"))
        # call("invalidate_texture", **identity, path="display.png")
```
