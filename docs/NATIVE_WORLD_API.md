# Native world API for Lua

These operations use `omsi.api(operation, arguments)`. They run synchronously on
the simulation thread. Read `omsi.api("snapshot", {})` first and pass its
`session_id` to every operation that acts on a specific object. A changed map
invalidates that session. Failed validation raises an API error before a batch
of writes is applied.

Set/trigger commands return bounded write receipts. Read the corresponding
`get` operation for a full state snapshot after a write.

This is access to openOMSI's actual objects. A scenery handle or native lane index
does not reproduce an OMSI executable memory address or runtime array offset.

## Scenery

`scenery.list` enumerates loaded scenery with running scripts/animations and
traffic-light objects. Arguments: optional `limit` (1–512, default 128),
`after_id`, `tile_index` (the global.cfg map list index), and `path_contains`.
The result contains `items` and `next_after_id`; pass the latter to fetch another
page. Every item has an `id` such as `scenery:123`, source `map_id`, tile and
`file_name`, position in metres, and `has_script`.

`scenery.get` takes `id` and returns `values`, `strings` and declared `triggers`.
`scenery.set` takes `id` and a `values` object and/or `strings` object. All names
must exist; numeric values must be finite. Name lookup is case insensitive and
duplicate names differing only by case are rejected. `scenery.trigger` takes
`id` and a declared trigger `name`.

```lua
local snapshot = omsi.api("snapshot", {})
local radars = omsi.api("scenery.list", {
    path_contains = "sceneryobjects/177/znacky/radar.sco"
})
for _, radar in ipairs(radars.items) do
    omsi.api("scenery.set", {
        session_id = snapshot.session_id,
        id = radar.id,
        values = { rychlost = 30 }
    })
end
```

Use the actual current vehicle speed in the radar's authored units in a real
plugin. Enumerate all pages if more than one page is returned. Unloading and
reloading an object gives it a new handle, even when its source map ID is the
same; stale handles are errors.

`scenery.placements.list` and `scenery.placements.get` also expose loaded source
placements tracked by the object editor, including static objects without
running scripts. This list uses `placement:N` handles and the same pagination,
with optional `tile_index`. The `get` result includes related `script_handles`.
Positions, current/source transforms (column-major matrices), clockwise
`heading_deg`, file names and source IDs are readable. `runtime_relocated` says
whether this loaded instance has received a native pose change. Editor-untracked
attachments and invisible source records are not part of this placement list.

`scenery.placements.set` takes `id`, optional absolute `position = {x,y,z}` in
world metres and/or absolute `heading_deg`. It moves the whole loaded object,
including its animated mesh transforms, collision mesh/boxes and collision grid,
camera blockers, script/HTML picking origin and map position index. Existing
pitch/bank are preserved; setters for them and deletion are not implemented.
The receipt reports the actual pose and `lifetime = "loaded_instance"`.
Unloading the tile discards the relocation and restores its authored indexed
position; loading it again uses the map file and a new handle. It does not write
map files or create a persistent editor edit. Existing editor edits are rejected.

This setter currently accepts only standalone ground placements remaining inside
their source tile. It rejects ambiguous IDs, incomplete map dependency indexes,
parents referenced by attached/varparent objects (including unloaded tiles),
terrain/surface/warped objects, roads/rail, stops/passenger places, trees, parked
cars, traffic signals/controllers, breakable poles, fuel/trigger zones, lights,
audio, particles and reflection cameras. Those classes require their owning
systems to rebuild; they are **remaining unsupported placement classes**. Network
play and cross-tile transfers are rejected. All validation precedes mutation.

## Traffic vehicles

`traffic.list` returns active AI vehicles with `traffic:N` handles. Pagination is
the same as scenery; `buses_only` optionally filters the list. Dormant and
removed cars are not active handles. The handle is unique to that live instance,
even if traffic is restarted in the same map. `simulation_id` separately exposes
the engine's car ID used by passenger and schedule relationships.

`traffic.get` returns actual position, clockwise heading in degrees, speed in
metres per second, lane and route, driver parameters and `script` containing
`values`, `strings` and `triggers`. `traffic.set` updates declared script/engine
variables using `values` and/or `strings`. `traffic.trigger` runs a declared
trigger. These operations do not accept arbitrary memory fields or pretend to
teleport a car by changing an unrelated script variable.

`traffic.behavior.get` and `traffic.behavior.set` access the AI driver's actual
parameters for the same handle. Set takes a `values` object with
`max_speed_kmh`, `accel_mps2`, `decel_mps2`, `lat_accel_mps2`, `headway_s`,
`min_gap_m`, and `desired_speed_factor`. A whole batch is validated before any
driver parameter changes; the next AI step uses the changed values.

`traffic.spawn_on_path` takes `path_id`, its listed `generation`, `distance_m`,
`file_name` relative to the content root (`Vehicles/.../*.bus` or `.ovh`), optional
`paint_scheme` (an index or `omsi.null` for the authored default), and optional
`initial_speed_mps` (default zero). It uses the normal vehicle loader, script
initialization, renderer, trailers and native AI placement. Path/type mismatch,
invalid paint, an occupied location or unavailable renderer is an error. The
normal AI planner can reduce the requested initial speed; the result reports
the actual created vehicle. The initial asset load is synchronous and may take
longer than a normal API read. A scheduled trip is not attached automatically.

`traffic.remove` takes the live `id`, evicts/cleans up associated passengers,
detaches timetable ownership, releases native render/sound resources and removes
the actual car. The current scheduled departure stays recorded as spawned, so
removal does not immediately recreate it. Later departures run normally.

`traffic.service.get` reads the actual bus service phase, timing, destination and
upcoming stop records. `traffic.service.hold` takes `seconds` and extends boarding
only while the bus is actually boarding. `traffic.service.set_departure` takes
`departure_seconds` and updates both the current stop and service deadline while
boarding/waiting; other states reject the command. These commands do not assign
arbitrary state-machine numbers.

Traffic mirrored from a network host is readable; local mutations are rejected
because that host owns its simulation.

## Light controllers

`traffic.lights.list` returns real crossing controllers. Its pagination uses
`after_object_id` and returns `next_after_object_id`; these IDs are decimal
strings. `traffic.lights.get` and `traffic.lights.set` use `object_id` and the
opaque string `generation` returned by the list. Restarting traffic invalidates
the generation even when the map session stays the same.

Readable fields include the native `controller_index`, `time`, `held`, `cycle`,
effective `cycle_length`, `offset`, demand `requests`, phase lists `lights`,
`approach` distances and conditional `stops`.

Writable fields are `time`, `held`, `requests`, `cycle`, `offset`, `lights`,
`approach` and `stops`. Each phase is `{state=integer, duration=seconds}`. Each
stop/jump is `{light=zero_based_index, time=seconds, if_request=boolean,
jump_to=seconds_or_nil}`. The native number of lights cannot change. Phase
durations, array lengths, stop light references and every field type are checked
before committing any part of an edit. Seeking or editing a program clears its
cached stop-point state.

Demand requests are combined with the traffic's own requests after automatic
demand collection and affect the next simulation step. A true `held` pauses that
next step; renew it for a sustained external hold. This keeps a forgotten plugin
hold from permanently freezing a crossing. False requests do not suppress
genuine vehicle/pedestrian demand. Other program edits persist until changed or
the map/traffic world is replaced.

## Traffic paths

`traffic.paths.list` returns native lanes with `path:0`-style handles and a
network `generation` string. Preserve this token without parsing it. It accepts
`limit`, `after_index`, and optional
`tile={x,y}`. The cursor is `next_after_index`. `traffic.paths.get` and
`traffic.paths.set` require the listed `generation` as well as `id` and session.
Relist after a generation error.

Get exposes sampled points, headings, curvature, length, source tile/map/path
identity, successor/predecessor lanes, geometric conflicts, actual reservations,
traffic-light association, and rules. A controller association uses the same
`controller_index` exposed by the light list.

Set accepts a `values` object with `speed_limit_kmh`, `priority`, `density`,
`no_cars`, `no_trucks`, and `turn` (0 none, 1 left, 2 right). Changes refresh the
native spawning weights and reachable-network distances. Unsupported properties
are errors, including geometry/topology writes that would require rebuilding
the network. A lane closure affects route choice; it does not despawn cars
already on that lane.

## Date and network lifetime

A native calendar change preflights the target timetable and dated AI fleet.
When the active Chrono scenarios change, old AI buses/cars and their ground
passenger routes are retired, road/light caches are cleared and visible tiles
are streamed again from the new sources. Passenger ownership inside player and
placed buses is preserved. Traffic path/controller generations change; plugins
must enumerate fresh handles before writing again.

Pending tile jobs and partially uploaded tiles from the old date are cancelled.
The date command does not wait for all replacement GPU tiles to finish loading;
the regular streamer fills them over subsequent frames. The active traffic graph
contains only the new generation while this happens.
