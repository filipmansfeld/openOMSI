# Native game API (development)

Lua plugins call the simulator directly through `omsi.api(operation, arguments, binary)`.
The optional external bridge calls the same engine dispatcher. No original OMSI process,
memory offsets, OmsiHook DLL, or loopback server is needed for Lua.

This is an implementation in progress. The operations below are the current native
surface, not a replacement for every OmsiHook operation. In particular, raw Delphi
objects, Direct3D COM pointers, arbitrary process memory, and all of OMSI's internal
passenger/path states are not equivalent to native openOMSI objects.

Detailed current contracts: [core, clock and timetable](NATIVE_CORE_API.md),
[scenery and traffic](NATIVE_WORLD_API.md),
[vehicle physics and weather](NATIVE_VEHICLE_ENVIRONMENT_API.md), and
[audio and particles](NATIVE_AUDIO_PARTICLES_API.md).

## Calls and errors

```lua
function on_frame()
    if not omsi.has_vehicle() then return end
    local snapshot = omsi.api("snapshot")
    local vehicle = omsi.api("vehicle.get")
    -- Keep the whole identity when retaining a vehicle between callbacks.
    local identity = {
        session_id = snapshot.session_id,
        id = vehicle.id,
        generation = vehicle.generation
    }
    omsi.log(vehicle.friendly_name, vehicle.speed_metres_per_second)
end
```

`arguments` is a table with string keys. Nested arrays use consecutive indices starting
at 1. Array elements and explicitly unavailable fields use `omsi.null`; assigning Lua
`nil` would remove an element. Numeric API indices inside a record (such as a HOF or
script-texture index) remain zero based.

Use `omsi.array{}` (or `omsi.array()`) for an empty array; plain `{}` means an object.
For example, `stops = omsi.array{}` clears a traffic controller's stop/jump program.
Arrays returned by the engine retain their array type when passed back, even when
empty. `omsi.array{1, omsi.null, 3}` explicitly marks a populated array. Mixed keys
and gaps still fail; the helper cannot convert `omsi.null` or a table with a custom
metatable into an array.

The optional third argument is a Lua binary string, used for texture pixels. It is
passed as bytes, not converted to UTF-8. Other strings must be UTF-8. A call accepts at
most 16 MiB of binary data, 2 MiB of text and keys, 65,536 values, and 24 nesting levels.
Cyclic tables, mixed array/object keys, missing array entries, nonfinite numbers,
functions and userdata are rejected before dispatch.

Calls execute synchronously on the simulation thread. A normal return from a write
means the engine applied it or accepted a documented command for the next simulation
step; queued commands return `queued = true`. Invalid values, stale handles and
unsupported operations raise a Lua error; use `pcall` when a disappearing object is
expected. If a completed operation's reply exceeds conversion limits, the error
explicitly says that state may already have changed; do not blindly repeat a mutation.
Do not perform large reads every frame. Calls belong in game callbacks; initialization
before a game context exists does not provide the native API.

```lua
local ok, result = pcall(omsi.api, "vehicle.get", identity)
if not ok then
    -- Drop the old handle and obtain a new snapshot/vehicle identity.
    omsi.warn(result)
end
```

## Identity and units

* `snapshot.session_id` identifies the loaded map session. Obtain a new snapshot after
  a map change. Pass the session with retained identities.
* `vehicle.list` and `vehicle.get` return opaque decimal string IDs and generations.
  Preserve them as strings. Explicit vehicle writes require the matching generation.
  Without an ID, a vehicle operation targets the current player vehicle.
* Scenery, AI traffic, light and path handles belong to the current session. A loaded
  instance's handle is not its place in OMSI's runtime arrays. World object access
  requires a session except for enumeration.
* Native `world_position` uses east, north, up in metres. The compatibility `position`
  field uses tile-local OMSI x, y-up, z-north. Heading in the vehicle snapshot is radians;
  the older `omsi.position()` helper returns degrees. Speed fields state their units.
* `clock.service_seconds` is simulation seconds since midnight. `day_of_year` follows
  the simulator's day numbering; calendar `day`, `month` and `year` are also provided.
  Fractional seconds are retained.

## Vehicles, clock, depots and displays

| Operation | Read/write | Arguments and result |
| --- | --- | --- |
| `snapshot` | Read | Session, clock, active vehicle, map and implemented capability labels. |
| `clock.get` | Read | Calendar and simulation clock. |
| `clock.set` | Write | `service_seconds`, `year`, `day_of_year`; the entire update is validated before application. |
| `vehicle.list` | Read | Active and placed player vehicles, with IDs and generations. AI traffic is a separate list. |
| `vehicle.get` | Read | Identity, position, controls, script variables, strings, triggers, selected HOF and available texture slots. |
| `vehicle.set_variables` | Write | `values = {name = number}` and/or `strings = {name = text}`. Every name is validated before any value is written. |
| `vehicle.trigger` | Execute | `name`: execute a real script trigger. An unavailable trigger is an error. |
| `vehicle.set_speed` | Write | `metres_per_second`: set the selected vehicle's speed. |
| `hof.list` | Read | Depot file indices/names and the selected depot. |
| `hof.get` | Read | `index`: read the actual parsed depot including stops, trips, destinations and strings. |
| `hof.set` | Write | `index`: load and select a depot for that vehicle. |
| `timetable.get` | Read | Current player duty and basic next-stop data. This is not the complete original TTData API. |
| `texture.upload` | Write | `section`, `index`, `width`, `height`, `format = "bgra8"` plus packed pixels as the third argument. |
| `texture.release` | Write | Release an externally driven script texture by section and index. |
| `texture.invalidate` | Write | `path`: refresh a file texture within the selected vehicle's texture folders, such as a generated driver card. |

Texture section 0 is the leading vehicle; subsequent sections are its coupled parts.
Script texture indices are the actual model slots, not a fixed navigation/LCD convention.
Uploads are bounded to 2048 by 2048 pixels and must contain exactly width × height × 4
bytes. Script rendering does not overwrite a texture while an external producer owns it.

## Scenery and traffic

| Operation | Read/write | Purpose |
| --- | --- | --- |
| `scenery.list` | Read | Enumerate loaded scripted objects and traffic lights; optional `tile_index`, `path_contains`, `after_id`, `limit`. |
| `scenery.get` | Read | Read the live object's variables, strings and triggers using its `id`. |
| `scenery.set` | Write | Set named `values` and/or `strings` atomically after validation. |
| `scenery.trigger` | Execute | Execute the object's named script trigger. |
| `traffic.list` | Read | Enumerate active native AI traffic vehicles. |
| `traffic.get` | Read | Read the selected AI vehicle and its available script state. |
| `traffic.set` | Write | Write supported script state; unknown fields are errors. |
| `traffic.trigger` | Execute | Execute the selected AI vehicle's script trigger. |
| `traffic.lights.list` | Read | Enumerate native traffic-light controllers. |
| `traffic.lights.get` | Read | Read a controller's actual native phase/request state. |
| `traffic.lights.set` | Write | Update supported controller fields after validating the full request. |

Paged lists contain `items` and `next_after_id`. Pass the returned continuation value
as `after_id` until it equals `omsi.null`. Enumerations describe currently loaded native
instances, not every map file on disk. Numeric source IDs and file paths are included
where available to locate map-specific objects without guessing runtime array indices.

## People and passenger entry selection

| Operation | Read/write | Arguments and result |
| --- | --- | --- |
| `humans.list` | Read | Optional `after_id` and `limit` (1–512, default 128). Returns `items`, `next_after_id` and the human simulation generation. |
| `humans.get` | Read | `id`: read one currently loaded person. |
| `humans.set` | Write | `id` and one or more of `pace_metres_per_second`, `speed_metres_per_second`, `heading_degrees`, `state_seconds`, `waiting_patience_seconds`, `exit_stop`. The complete update is validated before any field changes. |
| `humans.reassign_entry` | Write | `id`, zero-based `entry`: change the entry used by a passenger already queueing for that same bus. |
| `humans.assign_seat` | Write | `id`, zero-based `seat`: reserve another free passenger place in the same bus and make a riding passenger walk to it. |
| `humans.control` | Write | `id`, `speed_metres_per_second` (0–5), `heading_degrees`: desired motion for the next human simulation step. |
| `humans.fares.list` | Read | Active ticket catalogue; optional `offset` and `limit` (1–256). Returns product indices, prices, names, age ranges and pack selection parameters. |
| `humans.ticket.get` | Read | `id`: actual buy/pass/validator decision, selected product and that person's active player cash-desk transaction, when applicable. |
| `humans.ticket.set` | Write | `id`, `mode = "buy"`, `"pass"`, `"stamp"` or `"auto"`; `ticket_index` is required only for `buy`. Changes the decision used by boarding and cash-desk routing. |

Human handles have the form `human:<generation>:<native id>`. Preserve the complete
string and pass `session_id` from the current snapshot for get/write operations.

Ticket choice is writable only before boarding. A purchase must select a current
product within the passenger's age range. For someone already in a queue, the chosen
entry must support ticket sales and have a cash desk, or the bus must have a usable
validator for `stamp`. `auto` returns an unqueued passenger to the normal age-weighted
decision at boarding. These operations preserve platform and seat reservations and
reject changes during an active payment. Cash amounts are read from the actual shared
desk only for the person using it; they are not independent writable money fields on
every passenger. The native modes do not claim to reproduce the original TicketType
integer values.
Generation changes prevent an old handle from selecting a different person after a
map reload. A disappearing person or stale handle raises an error.

The read result describes the actual native state (`waiting`, `queue`, `riding`, etc.),
bus association, world position in east/north/up metres, heading in degrees, floor
frame velocity, preferred pace and intended exit. It does not assign OMSI's numeric
AI mode values to unrelated native states. Ground velocity uses east/north; velocity
inside a bus uses right/forward in its cabin frame.

The result also includes native activity, model dimensions, age, selected ticket,
boarding/alighting choices and movement/waiting timers. These are native facts; similar
names in an OMSI memory structure do not establish identical indexing or enum values.

`pace_metres_per_second` (0.1–5) changes preferred walking pace. The movement system
still slows people for obstacles, queues and doors. `speed_metres_per_second` (0–5)
changes instantaneous floor-frame velocity, preserving direction (or using the person's
heading when stationary); the next simulation step may replace it. Only walking states
accept instantaneous speed. Remote people, player avatars and test puppets reject writes.
Heading is a world heading in degrees; inside a bus its local heading is updated using
the current articulated section pose. `state_seconds` changes elapsed time in the
current native state (0–86,400). `waiting_patience_seconds` uses the same bounds and
requires the `waiting` state; normal simulation still decides the next transition.

`exit_stop` is a current player timetable stop index, or `-1` to restore the engine's
random stop selection. It is writable only for a passenger already inside the player's
bus who has not started alighting. A timetable index is checked against the actual
stop-ID list, and both the index and resolved stop identity are updated together.

Entry reassignment requires the passenger to remain on the ground in the queue, the
same bus to be stationary at the same stop, the requested entry to exist and be open,
and a ticket buyer's new entry to support ticket sales. The waiting-place ownership
and cabin reservation table must still be valid, with a free passenger place. The
operation changes the queue target and leaves seat and doorway reservations to the
normal boarding simulation. It does not teleport a person, rewrite a rider's seat,
transfer them between buses or force an arbitrary AI state.

Seat reassignment requires a riding passenger whose current place is reserved, a
matching live cabin and an unreserved target reachable through its walking network.
The request rejects inconsistent duplicate claims, alighting passengers and occupied
places. It reserves the target, releases the previous place, stands the rider on the
current seat's floor, and starts the normal walk-to-seat state. Position is not moved
to the target seat. This is a native transition rather than an arbitrary writable
OMSI seat pointer.

`humans.control` queues one movement command for the next simulation step. Its heading
is in the current floor frame: north on the ground, forward inside the bus. The command
enters the same desired-motion pipeline as the person's AI, before sitting/standing
constraints and crowd collision avoidance. It preserves the path goal, aisle corridor
and queue following. Renew the command each simulation step for sustained control.
Changing to a nonwalking state or another floor invalidates the pending command.
`motion_command_pending` reports the queue; `desired_motion` reports the last actual
desired velocity/heading entering that pipeline, or null before the first update.

## Driver walking

`driver.walk.get` reads the actual local walker, or `{active=false}` when seated at the
wheel. Active walkers have a `walker:<lifetime>` identity. The result includes the
world position and velocity, view angles, ground/jump state, native bus/cabin or seat
association, and whether a door transition or API command is pending.

`driver.walk.start` uses the simulator's normal getting-up action. A player vehicle,
loaded map and enabled **Ability to get up** setting are required. `driver.walk.stop`
uses the normal **Back to my bus** action, returning to the existing player's wheel.
Stop and control require the active walker `id`; all writes require current `session_id`.
A stale walker handle fails even when another walker started in the same map session.

`driver.walk.control` accepts `forward` and `right` in −1..1, optional
`speed_metres_per_second` in 0..10 (default 1.45), optional `heading_degrees`, and
optional `jump`. Axes use the walker's view frame and diagonal input is capped to the
requested speed. The command lasts one simulation step. Normal acceleration, ground,
world collision, cabin walking and open-door checks still decide the resulting motion.
Controls require an unpaused, standing walker in first-person view; door transitions,
seats and free camera reject them. Jump additionally requires outside ground contact.

This exposes native driver walking. The original custom `ConfigureDriverWalkV1–V4`
collision-box/portal formats, launcher process supervision and injected original OMSI
camera hooks do not yet have a binary-compatible implementation.

## Lifecycle events

Lua emits `start`, then the available `map(name)`, `renderer(true)` and
`vehicle(name)` events in that order when a plugin starts. At each frame it checks
map identity, renderer presence and active vehicle identity, in that order,
before timers, watches and the `frame` callback. Replacing a map or vehicle with
another instance of the same name still emits its event. Removing a map or
vehicle sends `nil`; losing the renderer sends `false`. `stop` runs before
`omsi.data` is saved.

The map event describes the current world/session, not individual tile loading
or separate map-load-start/map-load-complete stages. The renderer event describes
whether an engine renderer exists; it is not a Direct3D device-loss/reset event.
Retained API identities must still be refreshed after their lifetime changes.

## Verification and scope

`cargo test -p omsi-plugin --lib --test native_api --test lua` exercises the Lua binding,
bounded conversions, errors, synchronous write/read behavior and existing plugin lifecycle.
Engine-specific tests live beside the dispatcher and scene helpers. The API inventory
of the actual OmsiHook assembly is tracked separately; matching names or signatures is
not evidence that behavior has been implemented or tested.

Wheelchair-specific configuration and passenger lifecycle operations are not exposed
by the current native API. General passenger creation/removal, complete boarding and
alighting control, and other unimplemented contracts are listed separately in the
[remaining-contract assessment](NATIVE_CORE_API.md#remaining-original-api-contracts-development-assessment-2026-10-01).
