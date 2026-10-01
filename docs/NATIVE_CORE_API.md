# Native core API

Lua calls `omsi.api(operation, arguments, binary)` on the game thread. The optional
third argument is a binary Lua string. Failed operations raise a Lua error. Writes
finish before the call returns. API indices are zero-based, including indices
inside arrays returned as ordinary one-based Lua tables.

External clients use the same dispatcher through the opt-in local bridge. A named
operation has `op`, `session_id`, and an `args` object; its response carries `result`.
The existing bridge `snapshot` operation retains its `snapshot` response field.
This is a native engine API; it does not expose an OMSI process address space.

External command IDs must increase within a connection. Retrying the most recent
command with its same ID retrieves its receipt without executing again. Older IDs
are rejected even after their receipt has been discarded. A command that times out
while queued is cancelled before execution. A timeout after it started returns
`status: "indeterminate"`; retry that ID on the same connection to retrieve the
eventual result. Disconnecting loses that receipt, so inspect current state before
issuing a new mutation on a new connection.

## Identities and bounds

`snapshot` returns the current `session_id`, selected vehicle and clock. A map
replacement changes the session. `vehicle.list` and `vehicle.get` return decimal
string `id` and `generation` values so Lua does not lose integer precision. An
explicit vehicle ID requires its generation for every write. Omitting both selects
the current player, which is useful for plugins intentionally following that player.
`scenery.*`, `traffic.*` and `humans.*` require the current session except list calls.
Any supplied stale session is rejected for every operation.
The snapshot's `native_operations` lists implemented operation names. An operation
may still require a loaded map, vehicle or graphics device; its presence does not
imply arbitrary read/write support for every field of that domain.

Requests are limited to 2 MiB of JSON and 16 MiB of binary data. Numeric values must
be finite. Script variable batches allow 512 names; names are at most 256 bytes and
string values at most 65,536 bytes. Numeric and string batches are checked in full
before writing. Unknown variables and ambiguous case variants are errors.

## Clock

| Operation | Arguments | Result |
| --- | --- | --- |
| `clock.get` | `{}` | Hour, minute, fractional second, day, month, year, day of year and seconds since midnight |
| `clock.set` | Any of `service_seconds`, `year`, `day_of_year` | Updated clock |

Years are 1–9999, day of year is 1–365 or 366, and seconds since midnight must be
in `[0, 86400)`. Invalid partial updates fail rather than clamp a date. Clock writes
update simulator, traffic and vehicle script clocks. Date changes prebuild the
target-date Chrono timetable and replan the player's duty before committing. They
replace the previous scheduled AI fleet and request the normal Chrono/season tile
reload. When the active Chrono scenarios change, all AI traffic and ground
pedestrians are retired and the road/controller graph is rebuilt from that date's
files; passengers inside the player's bus remain and their destinations are
reconciled with the new duty. If the previous duty does not exist on the new date,
the result includes `cleared_duty_reason`. Date changes also return a new session.
Changed Chrono ticket packs are loaded before the transition. A changed fare
catalogue is rejected while a cashdesk transaction is in progress; unchanged
catalogues remain usable. Successful changes rebind passengers' selected products
and vehicle ticket data to the new catalogue.
Calendar changes during LAN sessions are explicitly rejected until coordinated
host/map transitions are available; no partial local date write is performed.

## Vehicles and script data

`vehicle.list` lists the player and placed buses. AI traffic has its own `traffic.*`
operations. `vehicle.get` accepts an optional vehicle ID and returns actual script
variables, strings, triggers, controls, pose, speed, HOF data and texture descriptors.
It reports the selected player's basic duty data; another placed bus has a null duty.

| Operation | Arguments in addition to optional vehicle identity |
| --- | --- |
| `vehicle.set_variables` | `values: {name: number}`, `strings: {name: string}` |
| `vehicle.trigger` | `name` |
| `vehicle.set_speed` | `metres_per_second` |

The legacy tile-local `position` uses OMSI axes: x east, y up, z north. `heading`
is radians. `world_position` uses explicitly named `east`, `north`, `up` components.
`speed_metres_per_second` is signed. Native physics and model commands have their
own supported fields; they are not arbitrary writes to memory offsets.

## HOF depots

| Operation | Arguments | Result |
| --- | --- | --- |
| `hof.list` | Optional vehicle identity | Existing depot files, zero-based index and selected flag |
| `hof.get` | Vehicle identity, `index` | Parsed depot data |
| `hof.set` | Vehicle identity, `index` | Selects that depot and returns a compact identity/name receipt |

Files come from the active vehicle's resolved depot directories. HOF responses
include name, service trip, global strings, string counts, termini, bus stops,
information trips and information bus-stop records. An information trip's numeric
`code` or `target` is null if its text is not an integer; `code_text` and `route`
retain the parsed text. HOF file order is the engine's resolved depot order.

## Textures

`vehicle.get` lists `script_textures` descriptors with `section`, `index`, `width`
and `height`. Section zero is the leading body; following sections are trailers.
Only authored slots with live GPU resources can be updated.

- `texture.upload`: `section`, `index`, `width`, `height`, `format: "bgra8"`, plus
  exactly `width * height * 4` binary bytes. Rows run top to bottom; alpha is retained.
  Each dimension is 1–2048, and slot indices are 0–63.
- `texture.release`: `section`, `index`, optionally the upload's `lease`. Restores
  the original script image or authored trailer blank and original dimensions.
- `texture.invalidate`: `path`, relative to the selected vehicle's texture folders.
  Refreshes an already resident file texture, including a PNG rewritten under the
  same name. Absolute paths and parent-directory traversal are rejected.

Uploads return a receipt containing vehicle identity, slot and lease. Releasing a
superseded lease fails. The bridge keeps receipts for its own uploads; disconnect
cleanup cannot overwrite a newer Lua upload. Material bindings keep their existing
texture resource IDs during resize.

## Timetables

Catalog lists accept `offset` (default zero) and `limit` (default 100, maximum 256).
They return `items`, `total` and nullable `next_offset`. Original engine indices
are retained.

| Lists | Single full record |
| --- | --- |
| `timetable.lines` | `timetable.line {index}` |
| `timetable.trips` | `timetable.trip {index}` |
| `timetable.stops` | Full stop records are in the list |
| `timetable.links` | `timetable.link {index}` |
| `timetable.tracks` | `timetable.track {index}` |

`timetable.duty` returns the actual planned trips, stops, arrival/departure times,
directions and progress, or null when no duty is active. `timetable.get` retains
the compact legacy bridge duty view. Its distance fields are straight-line world
distances, not an emulation of OMSI's internal route-distance fields.

- `timetable.assign {line, tour}` uses the engine's ordinary duty planning and
  destination setup and returns an error if the exact duty is unavailable.
- `timetable.clear {}` releases the player's duty and clears script timetable data.
- `timetable.skip_stop {index}` skips forward in the active trip using the engine's
  stop helper. Backwards skips and invalid indices fail.
- `timetable.start_at {trip_index, stop_index}` selects a valid trip/stop through
  the existing duty initialization helper and updates the destination.

Tour-file departures are explicitly named `departure_minutes`; planned duty times
are named `arrival_seconds`, `departure_seconds`, and `end_seconds`. Catalog data
describes the currently loaded timetable, including its active Chrono selection.

## Remaining original API contracts (development assessment, 2026-10-01)

This assessment describes the implemented `NATIVE_OPERATIONS` and remaining
contracts of the original OmsiHook API. It identifies work still needed; it is not
a certification of equivalent behavior or an exhaustive member-by-member audit.
Neither a count of native operations nor a count of original declarations establishes
a coverage percentage.

Original class/member names below identify the contracts being compared. An existing
read operation does not imply a setter, and a shared name does not establish identical
units, lifecycle, side effects or reference ownership. The linked native contracts
define the supported arguments, limits and ownership of each implemented subset.

| Remaining contract | Original member anchors | Current native subset and required engine counterpart |
| --- | --- | --- |
| General passenger creation, routing and state transitions | `OmsiRemoteMethods.OmsiCreateWaitingPassenger`, `OmsiCreateStandingPassenger`, `OmsiSetHumanWaitingForBus`, `OmsiSetHumanAIModeEx`; `OmsiHumanBeingInst.MyBus`, `Target_Station`, `TicketIndex`; `OmsiVehicleInst.EntriesReq`, `ExitsReq`, `SeatOccupancy` | Existing-human reads, walking control, limited property updates, entry reassignment, seat transfer and ticket-decision changes are implemented. Creation/removal, assigning a new waiting destination or bus, complete boarding/alighting transitions and direct door-queue requests need native commands that preserve seat, waiting-place and queue ownership. Wheelchair-specific passenger and vehicle configuration/lifecycle contracts remain unsupported. |
| Map/tile content and placement mutation | `OmsiMap.Kacheln`, `GroundTypes`, `Chrono`; `OmsiMapKachel.ObjectsFromFile`, `SplineSegments`, `TerrainFromFile`, `WaterFromFile`; `OmsiMapObjInst.Position`, `Rotation`, `Scale` | Scenery scripts and loaded placement metadata are accessible. `scenery.placements.set` changes position/heading of supported standalone objects within their source tile, updating drawing, collision, camera blockers, script/picking origins and the position index. Changes last for the loaded instance. Attachment-dependent, terrain/warped, road/rail, passenger/service and other system-owned classes are explicitly rejected; see [placement limits](NATIVE_WORLD_API.md#scenery). Persistent map edits, cross-tile transfers, pitch/bank/scale setters, deletion and a complete terrain/water/spline API remain unsupported. |
| Road and rail topology, switches and reservations | `OmsiPathSegment.Next`, `Previous`, `PathLine`, `SwitchDir`, `Third_Rails`, `Crossings`, `Blockiert`; `OmsiMap.RealSwitches` | Native path geometry/connectivity reads, selected lane rules and light programs are available. Topology/geometry editing, complete rail switch and third-rail control and reservation mutation need graph rebuilds, remapped live routes and fresh generation tokens. |
| General textures, materials, meshes and reflection state | `D3DTexture.CreateD3DTexture`, `CreateFromExisting`, `UpdateTexture(updateArea, level)`, `Levels`; `OmsiMaterialMan.MaterialItems`; `OmsiRemoteMethods.OmsiSetReflectionArrayLengthAsync`, `OmsiSetReflectionDoorCameraRadiusAsync`, `OmsiSetVehicleShadowsAsync`, `OmsiSetNameplateAsync` | Authored script-texture slots support full BGRA uploads/release and resident-file invalidation. Arbitrary allocation/attachment, mip levels, partial-region updates, material mutation, shadow/reflection/nameplate controls and mesh/animation editing need renderer-owned resources and explicit lifetimes. Raw D3D addresses are not native texture handles. |
| Audio and particles | `OmsiRemoteMethods.OmsiSoundTrigger`; `OmsiSound.Playing`, `Pitch`, `VolFaktor`, `SndPos`, `Loop`; `OmsiSoundPack.Sounds`; `OmsiPartikelEmitter.Frequency`, `Position`, `Lebensdauer`, `Textur` | Vehicle/placed-section sound definitions, actual mixer playback/stop/reset, triggers, emitter definitions and live particles now have bounded native read/write operations; see [the sound/particle contract](NATIVE_AUDIO_PARTICLES_API.md). AI/scenery ownership, DirectSound/reverb state and original scheduler flags, per-emitter particle textures and original attachment/visibility/spotlight state remain unsupported. Native pitch multipliers do not emulate the original boolean `Pitch` member. |
| Physical configuration and articulated constraints | `OmsiRoadVehicle.Achse`, `Couple`, `Coupling`; `OmsiRoadVehicleInst.Achsen`, `Fuel_Percent`, `CoupleForce`, `WearLifespan_Var`; `OmsiPhysObjInst.Contact`, `LastForce` | Physical controls, pose, velocity, force/torque and solver observations are implemented. `vehicle.physics.parameters/configure` reads/changes the leading body's live mass, inertia, centre of gravity, rolling/turning coefficients and indexed wheel geometry, spring, damping and tyre coefficients; see [physical coefficient limits](NATIVE_VEHICLE_ENVIRONMENT_API.md#live-physical-coefficients). Shared definitions, wheel topology, trailer constraints, original fuel/wear state and complete coupled-section control remain unsupported. Measured force/contact fields are observations, not arbitrary writable solver inputs. |
| Complete timetable and HOF state | `OmsiTimeTableMan.RVFiles`, `NoRVNumbers`, `Invalid`, `AllStationIndicesResetted`; `OmsiVehicleInst.AI_Scheduled_Profile`, `AI_Scheduled_StartDay`, `AI_Scheduled_NextBusstopDist`; `OmsiHOF.Name`, `ServiceTrip`, `TargetStringCount`, `BusStopStringCount` | Catalogs, duty assignment/progress, selected service controls and depot selection are available. Vehicle-pool catalog/state and the full scheduled-service contract remain incomplete. Legacy bridge stop distances are straight-line distances. HOF selection is implemented, but editing the selected HOF's writable metadata/counts requires validated copy-on-write depot data and refresh of dependent displays/routes. |
| Tickets, drivers and service records | `OmsiGlobals.TicketPack`, `Drivers`, `SelectedDriver`, `OmsiTTLogs`; `OmsiRoadVehicleInst.Ticket_Give`, `Ticket_Items`, `Ticket_Passenger` | Chrono ticket-catalogue transitions, paged fare-catalogue reads and per-passenger ticket-decision reads/updates are implemented. The decision setter follows the normal boarding/cash-desk flow and rejects an ongoing payment. It does not replace a complete cash transaction or catalogue-editing API. Driver profiles/selection and career/service records also need dedicated native contracts. |
| Script curve boundary rules and wrapped name dictionaries | `OmsiConstBlock.Consts_str`, `Funcs_str`; `OmsiFuncClass.PreNullIsNull`, `AftNullIsNull` | Variables, triggers, program/constant/curve reads and curve evaluation are supported. The two original curve boundary flags have active setters and still need corresponding runtime semantics. `Consts`, `Funcs` and `Pnts` have getter-only array properties; they are not evidence of a required array-replacement setter. Returned wrapped curve objects and name dictionaries retain their separate mutation contracts. General live program recompilation is an additional capability, not established by those getter-only declarations. |
| Full environment, camera and driver-walk behavior | `OmsiWeather.LightAll`, `LightAmb`, `CloudPos`, `CloudTransparenz`; `OmsiActuWeather.Active`, `ICAO`; `OmsiRemoteMethods.ConfigureDriverWalk`, `ConfigureDriverWalkV2`, `ConfigureDriverWalkV3`, `ConfigureDriverWalkV4`; `OmsiVehicleInst.HeadPos`, `HeadVeloc` | Active weather fields, ordinary view selection/look/zoom/free-camera controls and native driver walking are implemented. `vehicle.head/head.set` accesses the driver's actual spring displacement/velocity; seat displacement is readable. Seat-state writes, complete lighting/cloud state, live-weather service control, mirror/VR camera editing and original custom driver-walk configuration formats remain unsupported. |
| Lifecycle and automatic date transitions | `OmsiHook.OnMapChange`, `OnMapLoaded`, `OnOmsiGotD3DContext`, `OnOmsiLostD3DContext`, `OnActiveVehicleChanged` | Lua has start/stop/frame, identity-based map and vehicle events, and a renderer-presence event, with defined startup and per-frame ordering; see [lifecycle events](NATIVE_API.md#lifecycle-events). These do not distinguish asynchronous map-loading stages or signal D3D device loss/reset. The coherent Chrono graph/timetable/ticket transaction serves `clock.set`; automatic midnight still uses the older `follow_date` path and needs the same graph-change guarantees without resetting an unchanged overnight service. |
| Program/editor and low-level representation | `OmsiProgMan.SelObjPri`, `SelSplPri`, `HoverObj`, `Path_ActiveRule`, `IsSaved`; `Memory.ReadMemory`, `WriteMemory`; `OmsiRemoteMethods.OmsiGetMem`, `OmsiFreeMemAsync`, critical-section methods | Native editor selection/edit/save contracts are not implemented by the gameplay API. Process addresses, Delphi collections, COM pointers and locks require a separately specified compatibility representation if they remain part of a binary-consumer contract; Lua engine operations alone do not emulate them. |

Completion requires tests for each retained original contract's reads, writes,
state-transition side effects, units and stale-identity behavior. Passing current
engine tests establishes the implemented subset only. Original plugin/launcher
binary compatibility is a separate integration result and is not established by
this native operation list or by matching assembly declarations.
