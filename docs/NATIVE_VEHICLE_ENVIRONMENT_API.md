# Native vehicle physics and weather

### Cockpit pointer observations

`vehicle.input_events` reads actual accepted mouse presses and releases on vehicle
`[mouseevent]` meshes, including coupled sections. Supply `id` and `generation` from
`vehicle.get`/`vehicle.list`, optional `after` as an unsigned decimal sequence string,
and `limit` in 1–64 (default 64). Without `after`, the response contains no historical
events and returns the current cursor. Reuse `next_after` for the next read. Reads
are nondestructive, so each consumer maintains its own cursor. All sequence fields
are strings, including `next_after`, `last_sequence` and each event's `sequence`.

The instance retains its latest 128 events in chronological order. `missed=true`
means older events after the supplied cursor were overwritten. A future cursor,
invalid limit, stale generation, or old session is rejected. Unloading the vehicle
ends the event lifetime; obtain the replacement's identity and cursor before reading.
The snapshot advertises capability `vehicle_input_events` and operation
`vehicle.input_events` for both Lua and the authenticated local bridge.

Each event includes `section` (0 front, 1 first coupled section), the exact
`trigger`/`mesh` from the model, `kind` (`down`/`up`), `pressed`, simulation time,
and optional `uv` and `mesh_position`. Positions use the original mesh coordinates
(x right, y up, z forward) before animation, body transforms or world placement.
Coordinates are null for forgiving picks without an exact central-ray hit and for
releases without a ray. Releasing while keeping a momentary switch held still
reports the physical release but preserves the native no-`_off` behavior.
Keyboard events and API-generated script triggers do not enter this pointer stream.

Use `omsi.api(operation, arguments)`. Vehicle operations accept the identity described
in [NATIVE_API.md](NATIVE_API.md): an explicit `id` requires its `generation` for writes;
retained identities should also carry `session_id`. Omit the ID to follow the player.
Every update below validates all arguments before changing the engine.

## Vehicle operations

| Operation | Arguments | Behavior |
| --- | --- | --- |
| `vehicle.set_controls` | `controls` containing throttle, brake, clutch (0–1), steering (−1–1) | Queues physical controls for the next positive-duration simulation step, after normal input sampling; scripts observe these inputs. Repeat each frame for continuous control. |
| `vehicle.set_pose` | Optional `position = {east, north, up}`, `heading_degrees`, `pitch_degrees`, `bank_degrees`, `preserve_velocity` | Moves the model origin and rigid body together. All three position components are required when changing position. Clears old ground-contact caches and realigns coupled parts. Velocity is preserved unless explicitly disabled. |
| `vehicle.set_velocity` | Optional `velocity_world = {east, north, up}`, `angular_velocity_body = {right, forward, up}` | Sets physical linear velocity in m/s and angular velocity in rad/s. Requires rigid-body physics. |
| `vehicle.apply_force` | Optional `force_world = {east, north, up}`, `torque_body = {right, forward, up}` | Accumulates newtons and newton-metres for the next positive-duration physics step. Loads affect every substep of that step, then clear. Requires rigid-body physics. |
| `vehicle.physics` | Identity only | Reads current controls, speed, steering, wheels and native rigid-body properties including pending loads. |
| `vehicle.physics.parameters` | Identity only | Reads the current instance's rigid-body coefficients and indexed wheel configuration. |
| `vehicle.physics.configure` | `values` with body properties and/or indexed `wheels` updates | Applies validated coefficients to the live rigid-body solver and telemetry. Recomputes static loads and body frequency; preserves model origin, velocity and queued loads. |
| `vehicle.model` | Optional `offset` (default 0), `limit` (1–256, default 64) | Reads vehicle metadata, authored cameras, bounding box, mesh/material metadata, live transforms/visibility and coupled-part poses. `next_offset` continues mesh enumeration. |
| `vehicle.script` | Optional `kind` (`metadata`, `constants`, `curves`, `blocks`), `offset`, `limit` (1–256) | Reads the compiled program's actual constants/curve points, block identities and source files. Collections are paged. Constants are currently inlined at compilation, so live constant writes need a separate compiler change. |
| `vehicle.curve` | `name`, `x` | Evaluates the vehicle's actual compiled curve using the VM's interpolation, including clamping outside the authored range. Names are case-insensitive; unknown curves fail. |

Vector arrays are ordinary Lua arrays. World axes are east/north/up. Body axes are
right/forward/up. Heading is clockwise from north. Native mass is kilograms and
inertia is kg·m². `definition_mass` is the value in the content file, which may be
written in tonnes or kilograms; it is deliberately separate from physical mass.
Pose writes retain signed pitch/bank in −180–180 degrees. Vehicle and camera position
writes reject coordinates beyond the native tile-index domain (also capped at ±10⁹ m
horizontally and ±10⁶ m vertically), rather than overflow downstream calculations.
Speed and angular-velocity components are bounded to ±10,000 in their documented
units; applied and accumulated force/torque components to ±10¹². Values beyond these
limits fail before changing the vehicle. `vehicle.physics.pending_controls` reports a
queued input separately from the current-step controls. A one-step input also expires
on placed vehicles, which do not receive the player's keyboard/controller sampling.
Teleportation immediately refreshes the ground-height callback and clears any cached
rail binding; a rail vehicle attaches to tracks near its new pose on the next rail frame.

`vehicle.physics` exposes the solver's current observations. It does not imply every
returned field has a setter, or that an observation such as acceleration can replace
the physical causes of that observation. Full original OmsiHook physics semantics,
arbitrary legacy pointer writes, wheel topology changes, trailer-constraint and
shared-definition editing, and unsupported camera/model mutations remain separate
coverage requirements.

### Live physical coefficients

`vehicle.physics.configure` accepts `mass_kg` (500–1,000,000),
`inertia_body_kg_m2` (three positive diagonal components, each 1–10¹⁰, satisfying
the physical triangle inequalities and a maximum component ratio of 1000),
`center_of_gravity_body_metres` (three components in ±100 m),
`rolling_resistance_newtons` (0–10⁸), `rotation_point_long_metres` (±100 m),
and `inverse_min_turn_radius` (0–10 m⁻¹). These are instance-local native units;
authored definitions and other vehicles using the same type do not change.

`values.wheels` contains 1–128 partial records with an existing zero-based `index`.
Duplicate indices, unknown fields and invalid values reject the whole request.
Writable wheel fields are `attach_body_metres` (three components, ±100 m),
`force_lever_metres` (±20 m), `radius_metres` (0.05–10 m), `inverse_inertia`
(10⁻⁸–10), `spring_newtons_per_metre` (1–10⁷),
`damper_newton_seconds_per_metre` (0–10⁶), `maximum_force_newtons` (1–10⁸),
`tyre_stiffness_newtons_per_metre` (1–10⁸),
`tyre_damping_newton_seconds_per_metre` (0–10⁶), and boolean `driven`.
Wheel geometry changes invalidate old ground contacts. Rendered mesh geometry is
still authored content; this command edits physical geometry only. The solver
computes wheel steering from turning radius, pivot and wheel position every step,
and reads air-suspension factors from scripts; those observations are not persistent
coefficient setters. Static load distribution uses the engine's longitudinal model.

Both commands require the leading body's rigid solver. They do not yet configure
trailer constraints or edit shared vehicle definitions. Relevant original anchors
are `OmsiPhysObj.Mass`, `J_X/J_Y/J_Z`, `CoG`, `OmsiRoadVehicle.Achse`, `Drehpunk`
and `Inv_Min_Radius`; original axis/unit conversions still belong to the adapter.
Physical coefficient tests observe actual solver acceleration, mass/load changes,
pose/motion retention, invalid-batch atomicity and a Lua→engine round trip.

## Active camera

`vehicle.head` reads the real driver's head-spring displacement and velocity in
body axes right/forward/up, plus the driver's seat displacement. `vehicle.head.set`
accepts `position_body_metres` (three components, ±2 m) and/or
`velocity_body_metres_per_second` (three components, ±100 m/s). It changes the
current spring state; the normal head simulation continues on the next frame.
The selected-vehicle snapshot also contains `head_position` and `head_velocity`
with legacy x-right/y-up/z-forward field ordering. These are head displacements,
not absolute world camera coordinates. This supplies the launcher's `HeadPos.x`
read from actual simulation state.

| Operation | Arguments | Behavior |
| --- | --- | --- |
| `camera.get` | None | Reads the actual current camera, view mode, position, angles, FOV, clip planes, direction/up, active look/orbit and camera indices. When a surface exists, includes its aspect ratio and the native column-major, reversed-Z view-projection matrix relative to `matrix_origin`. |
| `camera.select` | `mode` (`driver`, `pax`, `outside`, `free`), optional `index` for driver/pax | Selects an existing view. Driver index addresses the authored camera list; passenger indices include articulated parts. Saves/restores the normal per-camera look state. A walking driver must return to the bus first. |
| `camera.set` | One or more fields below | Validates the entire update, then changes the camera and persistent look/zoom state used by the render loop. |

The free camera accepts `position = {east, north, up}`, `heading_degrees`,
`pitch_degrees` (−89–89), `roll_degrees`, `near_metres` and `far_metres` (positive,
near less than far). Vehicle cameras instead accept `look_yaw_degrees` and
`look_pitch_degrees` (−85–85). The outside camera additionally accepts
`orbit_metres` (3.5–40). All supported views accept `fov_degrees` (8–120).
Normal user input, head tracking and camera collision continue to act on these views.
The API does not overwrite user settings files. Walking views, VR poses, mirror-camera
editing and arbitrary matrix writes are separate contracts and are not claimed here.

## Active weather

| Operation | Arguments | Behavior |
| --- | --- | --- |
| `weather.get` | None | Active conditions, wetness, native street condition, retained preset metadata and cycle/transition status. |
| `weather.set` | `values` with one or more fields below | Applies the validated conditions immediately. Cancels a pending transition/weather cycle so it cannot silently replace the write. |
| `weather.presets` | None | Installed weather files and their parsed conditions. No download. |
| `weather.select` | `file_name` from the preset list | Selects an installed preset immediately and initializes road wetness from it. |

Writable fields are `visibility_metres`, `wind_direction_degrees`,
`wind_metres_per_second`, `temperature_celsius`,
`absolute_humidity_grams_per_cubic_metre`, `road_wetness` (0–1), `cloud_type`,
`precipitation_kind` (0 none, 1 rain, 2 snow), `precipitation_rate` (0–1), `snow`
and `snow_on_road`. Supported active cloud categories are `-1`, `Cumulus 1`,
`Cumulus 2`, `Cumulus 3` and `Overcast 1`.

Weather writes update player/placed vehicle script hosts, the ambient conditions
used by new vehicles, and the active state used by rendering. A cloud-category
change rebuilds the sky texture. Rain and drying continue to evolve road wetness.
Preset metadata such as pressure is retained from the content file; it is not
advertised as a live simulated writable field. A native weather write currently
requires a local session: coordinated multiplayer weather writes remain unsupported.

## Verification

The application tests include a real Lua plugin using the production `PluginIo` and
dispatcher to change vehicle variables, execute a compiled vehicle trigger, move the
rigid body, set its velocity, change active weather and set fractional time. The same
test checks that invalid batches and stale identities do not affect the replacement
vehicle. A solver test confirms accumulated forces change motion and clear after
one step. These tests do not certify all original OmsiHook consumers or visually
verify every installed vehicle/map.
