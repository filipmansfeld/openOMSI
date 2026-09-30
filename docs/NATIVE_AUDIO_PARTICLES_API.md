# Native vehicle audio and particles

These operations are part of `omsi.api`, and use the actual mixer and particle
simulation. They are a native engine interface, not a binary implementation of
every OmsiHook sound or particle member. Ownership currently covers the player and
placed vehicles, including their loaded trailer sections. AI and scenery owners
are not connected to these operations yet.

All operations accept the core vehicle identity and optional `section` (zero for
the leading body, one for the first trailer). Writes require the current
`session_id`, the core vehicle generation when naming an explicit vehicle, and
the additional `audio_generation` or `particle_generation` returned by
enumeration. These generations are decimal strings, not floating-point numbers.
They identify a particular loaded runtime set. Reads validate them when supplied.
Expired live-particle handles cannot refer to newly emitted particles.

Unsupported fields, invalid values and unknown variables fail before a property
batch changes state. Property changes use `values: {name: value, ...}`. Empty
batches fail. Coordinates use the engine convention: local x right, y forward,
z up; world x east, y north, z up. Distances are metres and times are seconds.

## Sound operations

| Operation | Additional arguments | Result or effect |
| --- | --- | --- |
| `audio.list` | `offset`, `limit` | Compact sound records, generation, `total`, nullable `next_offset`, `output_enabled`. Default limit 64; maximum 128. |
| `audio.get` | `index` | Definition, decoded clip metadata, actual current mixer voice and control state. |
| `audio.set` | `index`, `values` | Validated definition update; changed file is decoded before committing. Mixer parameters update on the next sound step. |
| `audio.trigger` | `name` | Queue a declared sound trigger through the vehicle's ordinary sound-event list. |
| `audio.play` | `index`, optional `looping` | Start an actual one-shot or looping mixer voice under explicit control. |
| `audio.stop` | `index` | Stop the voice and suppress automatic restarts until reset. |
| `audio.reset` | `index` | Stop explicit playback, restore the authored definition and return to ordinary script-driven control. |

`audio.set` supports `file_name`, `volume`, `pitch_multiplier`, `sample_rate`,
`pitch_reference`, `pitch_variable`, `loop_sound`, `no_loop`, `only_one`,
`position_local`, `direction_local`, `range_metres`, `viewpoint`, `triggers`,
`conditions` and `volume_curves`. Position and direction are triples or null.
An empty pitch variable disables that binding. Other variable names must exist
on the selected vehicle. New clip paths must resolve inside the content root.
An integer filename retains the engine's dynamic-file-entry convention; it does
not itself supply a decoded clip for explicit playback.

Conditions are `{variable, relation, value}` records using native relation codes
0 through 5. Volume curves are `{variable, points: [[x,y], ...]}` records with
strictly increasing x coordinates. Curve variable `"-1"` reads time since
activation; `"-2"` reads listener-facing direction. Up to 64 conditions, curves
or trigger names are accepted, with at most 128 points per curve. Unknown nested
fields fail. Volume accepts 0–16; the ordinary mixer volume stage clamps the
result to 0–1 before master gain. Pitch multipliers accept 0.001–64; the final
playback pitch is bounded to the same range. Viewpoint accepts native codes 0–7.
`loop_sound` selects the authored `[loopsound]` pitch configuration; ordinary
start/loop behavior still follows the engine's triggers and `no_loop` rule.
Use `audio.play {looping: true}` for an explicitly looping voice.

Explicit playback bypasses automatic start conditions and view selection while
retaining volume curves, spatial attenuation and cabin muffling. Ordinary named
and file triggers cannot override explicit playback or a stopped entry until
reset. A named trigger enters the shared native vehicle event list, so another
section declaring that same name may also react. One-shot control does not
restart after its voice finishes; looping control does.

`output_enabled` distinguishes an available output device from readable sound
definitions. `audio.play` and `audio.trigger` reject unavailable output before
starting or queuing playback. A playing voice can still be silent because of
gain, distance, curves, or listener configuration. `active_seconds` uses the
runtime's monotonic activation clock, not OMSI's raw `StartTime` representation.
`queued: true` on a definition update describes the next mixer update, while
the validated definition is committed immediately.

## Particle operations

| Operation | Additional arguments | Result or effect |
| --- | --- | --- |
| `particles.emitters.list` | `offset`, `limit` | Authored emitter indices, capacities, live counts and generation. Default limit 64; maximum 128. |
| `particles.emitters.get` | `emitter` | Full actual definition and native variable bindings. |
| `particles.emitters.set` | `emitter`, `values` | Atomic emitter update used by subsequent simulation steps. |
| `particles.emitters.reset` | `emitter` | Restore authored properties and the default 100-particle capacity. |
| `particles.list` | optional `emitter`, `after_id`, `limit` | Live records with `particle:<birth-id>` handles and nullable `next_after_id`. Default limit 100; maximum 256. |
| `particles.get` | `particle_id` | Current position, velocity, age, lifespan, colour, size and alpha data. |
| `particles.set` | `particle_id`, `values` | Atomic update of that actual live particle. |
| `particles.clear` | optional `emitter` | Remove live particles from the selected emitter or whole section. |

Emitter setters support `position_local`, nonzero `direction_local`,
`velocity_metres_per_second`, `velocity_all_round`, `frequency_per_second`,
`lifetime_seconds`, `brake_factor`, `gravity_factor`, `size_start_metres`,
`size_growth_metres_per_second`, `alpha_initial`, `alpha_final`, `color`,
`calculation_distance_metres`, `emissive` and `max_particles`.

Scalar bindings are numbers or `{variable: "existing_name"}`. A native range
accepts `{base: scalar_binding, variation: scalar_binding}`; a scalar shorthand
means zero variation. Velocity, life, braking, gravity, size, alpha and each of
the three colour components use ranges. Frequency uses a scalar binding because
the current simulator does not evaluate its authored variation. Reads preserve
the actual binding expressions. Script-variable values remain dynamic and are
evaluated by the ordinary simulation; input bounds on constant values do not
replace script evaluation with an API-owned value.

An emitter can hold 0–4096 particles, subject to a total capacity of 65,536 per
section. Lowering capacity immediately truncates excess live particles. Other
birth properties affect new particles; surviving particles keep the parameters
captured at birth. `emissive` is read by rendering for existing particles as
well. Calculation distance accepts 50–1,000,000 metres, matching the simulator's
minimum distance. Clear removes live particles but does not reset emission
timing or rearm an already fired burst. Reset restores the emitter definition;
it does not respawn or rewrite surviving particles.

Live setters support `world_position`, `velocity_world`, `age_seconds`,
`lifetime_seconds`, `size_start_metres`, `size_growth_metres_per_second`,
`alpha_initial`, `alpha_final`, `color`, `brake_factor` and `gravity_factor`.
Age must not exceed lifespan. Colour and alpha constants are in 0–1; velocity
components are bounded to 10,000 metres per second; brake is in 0–1.5 and gravity
factor in -1000–1000. Current interpolated alpha and size are read-only results
of the native age/lifetime parameters. Particle handles remain valid through
property changes, and become invalid immediately on expiry or removal.

## Original API anchors and remaining contracts

The audit used the actual active accessors in
`Omsi-Extensions-upstream/OmsiHook/WrappedOmsiClasses/`:

* `OmsiSound.cs`: writable `FileName`, `SampleRate`, `RefValue`, `Loop`,
  `OnlyOne`, `Flag_Viewpoint`, `Is3D`, `SndPos`, `VolDist`, `HasDir`, `Dir`,
  `VolFaktor`, `Playing` and `MayPlay` motivate native definition and playback
  operations. Native `pitch_multiplier` is a playback-rate multiplier;
  original `Pitch` is a boolean and is not that same property.
* `OmsiSoundPack.cs`: sound-pack enumeration provides the ownership anchor.
  `OmsiRemoteMethods.OmsiSoundTrigger` motivates actual named sound triggering.
  The pack's writable `FileName`, `Path`, `SoundCount`, `AI`, `RefRange` and
  `KoordSystem` are separate pack-level contracts, not implemented by an entry
  update or inferred from the getter-only `Sounds` array.
* `OmsiPartikelemitter.cs` declares `OmsiPartikelEmitter`: writable `Frequency`,
  `Lebensdauer`, `Lebensdauer_Variation`, `VeLoc`, `V_Variation`, `BremsFaktor`,
  `FallKoeffizent`, `StartSize`, `SizeGrow`, `Alpha_Initial`, `Alpha_End`,
  `Alpha_Variation`, `Farbe`, `MaxCnt` and `Position` correspond to supported
  native emitter concepts. Native direction plus speed is not an identical
  representation of the original `VeLoc` vector.
* `OmsiPartikel.cs`: writable position, velocity, life, size, alpha and colour
  concepts motivate live-particle updates. Native age/lifetime seconds are not
  a binary emulation of original birth/death timestamps or packed colours.

Unimplemented contracts remain explicit: DirectSound device/buffer/reverb/3D
interface pointers, reverb gain/time, buffer allocation state, original internal
variable arrays, raw start timestamps and scheduler flags; full behavior for
sound `Important` and `CheckLoading`; arbitrary sound-pack ownership or topology;
particle `LastEmitTime`, `X_Var`, `Y_Var`, `Spotlight_Calc`, `Textur` and the
original per-particle rotation/attachment/illumination/visibility state. The
current renderer uses its shared particle sprite, so exposing an authored
bitmap as metadata does not make per-emitter texture writes work. Attachment,
burst and frequency variation are read as authored metadata, without setters.

Getter-only returned arrays are not classified as missing array setters:
`OmsiSound.TriggerList` and `OmsiPartikelEmitter.Partikel` have no active property
setter. Wrapped objects or `MemArray` instances may have their own independent
mutation contracts; a getter alone is not evidence of write support or its lack.
Full original-plugin compatibility still requires integration tests beyond
these native operations.

Behavior tests exercise actual mixer voices without opening an output device,
time-curve activation, failed clip-change preservation, whole-batch rejection,
actual emission and particle motion, capacity enforcement and expired identity
rejection. These focused tests do not establish hardware playback or complete
original-plugin compatibility.
