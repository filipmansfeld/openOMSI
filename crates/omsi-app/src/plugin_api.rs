//! Shared, main-thread game API for Lua and external bridge clients.
//! Operations return actual application results; unknown operations and unavailable
//! data are errors, never synthetic successful writes or invented object records.

use crate::native_bridge::protocol::{decode_bgra, validate_values, ClockUpdate, Request};
use crate::{App, Player};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

// List the implemented operations, not broad claims of OMSI memory/API parity.
// An operation can still require a loaded map, vehicle or graphics device.
const NATIVE_OPERATIONS: &[&str] = &[
    "snapshot",
    "clock.get",
    "clock.set",
    "vehicle.list",
    "vehicle.get",
    "vehicle.set_variables",
    "vehicle.trigger",
    "vehicle.set_speed",
    "vehicle.set_controls",
    "vehicle.set_pose",
    "vehicle.set_velocity",
    "vehicle.apply_force",
    "vehicle.physics",
    "vehicle.model",
    "vehicle.script",
    "vehicle.curve",
    "vehicle.physics.parameters",
    "vehicle.physics.configure",
    "vehicle.head",
    "vehicle.head.set",
    "hof.list",
    "hof.get",
    "hof.set",
    "texture.upload",
    "texture.release",
    "texture.invalidate",
    "timetable.get",
    "timetable.duty",
    "timetable.assign",
    "timetable.clear",
    "timetable.skip_stop",
    "timetable.start_at",
    "timetable.lines",
    "timetable.line",
    "timetable.trips",
    "timetable.trip",
    "timetable.stops",
    "timetable.links",
    "timetable.link",
    "timetable.tracks",
    "timetable.track",
    "scenery.list",
    "scenery.get",
    "scenery.set",
    "scenery.trigger",
    "scenery.placements.list",
    "scenery.placements.get",
    "scenery.placements.set",
    "traffic.list",
    "traffic.get",
    "traffic.set",
    "traffic.trigger",
    "traffic.remove",
    "traffic.spawn_on_path",
    "traffic.behavior.get",
    "traffic.behavior.set",
    "traffic.service.get",
    "traffic.service.hold",
    "traffic.service.set_departure",
    "traffic.paths.list",
    "traffic.paths.get",
    "traffic.paths.set",
    "traffic.lights.list",
    "traffic.lights.get",
    "traffic.lights.set",
    "humans.list",
    "humans.get",
    "humans.set",
    "humans.reassign_entry",
    "humans.assign_seat",
    "humans.control",
    "humans.fares.list",
    "humans.ticket.get",
    "humans.ticket.set",
    "weather.get",
    "weather.set",
    "weather.presets",
    "weather.select",
    "camera.get",
    "camera.select",
    "camera.set",
    "driver.walk.get",
    "driver.walk.start",
    "driver.walk.stop",
    "driver.walk.control",
    "audio.list",
    "audio.get",
    "audio.set",
    "audio.trigger",
    "audio.play",
    "audio.stop",
    "audio.reset",
    "particles.emitters.list",
    "particles.emitters.get",
    "particles.emitters.set",
    "particles.emitters.reset",
    "particles.list",
    "particles.get",
    "particles.set",
    "particles.clear",
];

pub(crate) struct ApiState {
    session: String,
    // Weak handles preserve allocation identity without keeping unloaded maps alive.
    map: Option<std::sync::Weak<crate::scene::World>>,
    vehicles: BTreeMap<u64, (std::sync::Weak<omsi_sim::vehicle::VehicleType>, u64)>,
    next_generation: u64,
    sequence: u64,
    hof: Option<(Arc<omsi_vehicle::hof::Hof>, Value)>,
    texture_leases: BTreeMap<(u64, u64, omsi_render::TextureId), u64>,
    next_texture_lease: u64,
}

impl Default for ApiState {
    fn default() -> Self {
        Self {
            session: random_id(),
            map: None,
            vehicles: BTreeMap::new(),
            next_generation: 0,
            sequence: 0,
            hof: None,
            texture_leases: BTreeMap::new(),
            next_texture_lease: 0,
        }
    }
}

pub(crate) fn random_id() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl ApiState {
    fn refresh(&mut self, app: &App) {
        let map = app.world.as_ref().map(Arc::as_ptr);
        if map != self.map.as_ref().map(std::sync::Weak::as_ptr) {
            self.map = app.world.as_ref().map(Arc::downgrade);
            self.session = random_id();
            self.vehicles.clear();
            self.hof = None;
            self.texture_leases.clear();
        }
        let players: Vec<_> = app.player.iter().chain(app.placed.iter()).collect();
        self.vehicles
            .retain(|id, _| players.iter().any(|p| p.uid == *id));
        for player in players {
            let ty = Arc::as_ptr(&player.vehicle.ty);
            if self
                .vehicles
                .get(&player.uid)
                .is_none_or(|(previous, _)| previous.as_ptr() != ty)
            {
                self.next_generation = self.next_generation.wrapping_add(1).max(1);
                self.vehicles.insert(
                    player.uid,
                    (Arc::downgrade(&player.vehicle.ty), self.next_generation),
                );
            }
        }
        self.texture_leases.retain(|(id, generation, _), _| {
            self.vehicles
                .get(id)
                .is_some_and(|(_, current)| current == generation)
        });
        if let Some((hof, path)) = app.player.as_ref().and_then(|p| {
            p.vehicle
                .host
                .hof
                .as_ref()
                .map(|h| (h, &p.vehicle.ty.def.path))
        }) {
            if self
                .hof
                .as_ref()
                .is_none_or(|(cached, _)| !Arc::ptr_eq(cached, hof))
            {
                self.hof = Some((hof.clone(), hof_snapshot(hof, path)));
            }
        } else {
            self.hof = None;
        }
    }

    fn vehicle_id(&self, app: &App, args: &Value, writing: bool) -> Result<u64, String> {
        let requested_id = args.get("vehicle_id").or_else(|| args.get("id"));
        let id = requested_id
            .map(|v| {
                v.as_u64()
                    .or_else(|| v.as_str()?.parse().ok())
                    .ok_or("invalid vehicle id")
            })
            .transpose()?
            .or_else(|| app.player.as_ref().map(|p| p.uid))
            .ok_or("player vehicle is not loaded")?;
        let (_, generation) = self
            .vehicles
            .get(&id)
            .ok_or("vehicle is no longer loaded")?;
        if let Some(value) = args.get("generation") {
            if value.as_u64().or_else(|| value.as_str()?.parse().ok()) != Some(*generation) {
                return Err("stale vehicle generation".into());
            }
        } else if requested_id.is_some() && writing {
            return Err("generation from vehicle.list or vehicle.get is required for writes to an explicit vehicle id".into());
        }
        Ok(id)
    }

    fn snapshot(&mut self, app: &App) -> Value {
        self.sequence = self.sequence.wrapping_add(1);
        let generation = app
            .player
            .as_ref()
            .and_then(|p| self.vehicles.get(&p.uid))
            .map(|(_, g)| *g)
            .unwrap_or(0);
        snapshot_value(
            app,
            &self.session,
            generation,
            self.sequence,
            self.hof.as_ref().map(|(_, h)| h),
        )
    }

    fn execute(
        &mut self,
        app: &mut App,
        operation: &str,
        args: &Value,
        binary: &[u8],
    ) -> Result<Value, String> {
        if let Some(session) = args.get("session_id") {
            if session.as_str() != Some(self.session.as_str()) {
                return Err("stale session; read a new snapshot".into());
            }
        }
        if (operation.starts_with("scenery.")
            || operation.starts_with("traffic.")
            || operation.starts_with("humans."))
            && !operation.ends_with(".list")
            && args.get("session_id").is_none()
        {
            return Err(
                "session_id from the current snapshot is required for world object operations"
                    .into(),
            );
        }
        if !binary.is_empty() && operation != "script_texture" && operation != "texture.upload" {
            return Err("this operation does not accept a binary payload".into());
        }
        if operation.starts_with("humans.") {
            let stops = app
                .player
                .as_ref()
                .map(|p| p.vehicle.host.tt_stop_ids.as_slice());
            return app
                .humans
                .as_mut()
                .ok_or("human simulation is not loaded")?
                .plugin_api(operation, args, stops);
        }
        match operation {
            "snapshot" => return Ok(self.snapshot(app)),
            "clock.get" => return Ok(clock_snapshot(app)),
            "set_clock" | "clock.set" => {
                let mut clock_args = args.get("clock").unwrap_or(args).clone();
                if operation == "clock.set" {
                    validate_fields(args, &["clock", "service_seconds", "year", "day_of_year"])?;
                    if args.get("clock").is_some()
                        && ["service_seconds", "year", "day_of_year"]
                            .iter()
                            .any(|name| args.get(*name).is_some())
                    {
                        return Err("clock fields must be supplied either inside clock or at the top level, not both".into());
                    }
                }
                if let Some(fields) = clock_args.as_object_mut() {
                    fields.remove("session_id");
                }
                let update: ClockUpdate = serde_json::from_value(clock_args)
                    .map_err(|e| format!("invalid clock update: {e}"))?;
                let (date_changed, duty_cleared) =
                    crate::plugin_api_timetable::set_clock(app, &update)?;
                if date_changed {
                    self.session = random_id();
                }
                let mut result = clock_snapshot(app);
                result["session_id"] = json!(self.session);
                result["date_changed"] = json!(date_changed);
                result["cleared_duty_reason"] = json!(duty_cleared);
                return Ok(result);
            }
            "vehicle.list" => {
                return Ok(json!(app.player.iter().chain(app.placed.iter()).map(|p| {
                json!({"id":p.uid.to_string(),"generation":self.vehicles[&p.uid].1.to_string(),
                    "is_player":app.player.as_ref().is_some_and(|current|current.uid==p.uid),
                    "file_name":p.vehicle.ty.def.path.to_string_lossy()})
            }).collect::<Vec<_>>()))
            }
            "timetable.get" => return Ok(schedule_snapshot(app)),
            "set_scenery_variables" => {
                let request = legacy_request(operation, args)?;
                validate_values(&request)?;
                if !request.strings.is_empty() {
                    return Err(
                        "scenery string writes are not supported by this legacy operation".into(),
                    );
                }
                let world = app.world.as_ref().ok_or("map is not loaded")?;
                let values: Vec<_> = request.values.into_iter().collect();
                world.bridge_scenery_set(request.tile_index, request.object_index, &values)?;
                return Ok(Value::Null);
            }
            _ => {}
        }
        if let Some(result) = crate::plugin_api_environment::execute(app, operation, args) {
            return result;
        }
        if let Some(result) = crate::plugin_api_camera::execute(app, operation, args) {
            return result;
        }
        if let Some(result) = crate::on_foot::plugin_api::execute(app, operation, args) {
            return result;
        }
        if let Some(result) = crate::plugin_api_world::execute(app, operation, args) {
            return result;
        }
        if let Some(result) = crate::plugin_api_timetable::execute(app, operation, args) {
            return result;
        }
        let id = match operation {
            op if op.starts_with("audio.") || op.starts_with("particles.") => self.vehicle_id(
                app,
                args,
                !matches!(
                    op,
                    "audio.list"
                        | "audio.get"
                        | "particles.emitters.list"
                        | "particles.emitters.get"
                        | "particles.list"
                        | "particles.get"
                ),
            )?,
            "vehicle.get"
            | "vehicle.set_variables"
            | "set_variables"
            | "vehicle.trigger"
            | "vehicle.set_speed"
            | "vehicle.set_controls"
            | "vehicle.set_pose"
            | "vehicle.set_velocity"
            | "vehicle.apply_force"
            | "vehicle.physics"
            | "vehicle.model"
            | "vehicle.script"
            | "vehicle.curve"
            | "vehicle.physics.parameters"
            | "vehicle.physics.configure"
            | "vehicle.head"
            | "vehicle.head.set"
            | "hof.list"
            | "hof.get"
            | "hof.set"
            | "script_texture"
            | "texture.upload"
            | "script_texture_release"
            | "texture.release"
            | "invalidate_texture"
            | "texture.invalidate" => self.vehicle_id(
                app,
                args,
                !matches!(
                    operation,
                    "vehicle.get"
                        | "vehicle.physics"
                        | "vehicle.physics.parameters"
                        | "vehicle.head"
                        | "vehicle.model"
                        | "vehicle.script"
                        | "vehicle.curve"
                        | "hof.list"
                        | "hof.get"
                ),
            )?,
            _ => return Err(format!("unsupported game API operation: {operation}")),
        };
        if let Some(result) = crate::plugin_api_audio::execute(app, id, operation, args) {
            return result;
        }
        if let Some(result) = crate::plugin_api_particles::execute(app, id, operation, args) {
            return result;
        }
        if matches!(operation, "vehicle.head" | "vehicle.head.set") {
            return crate::plugin_api_camera::head(player_mut(app, id)?, operation, args);
        }
        if operation == "vehicle.get" {
            let player = player(app, id)?;
            let hof = player
                .vehicle
                .host
                .hof
                .as_ref()
                .map(|h| hof_snapshot(h, &player.vehicle.ty.def.path));
            let mut result = vehicle_snapshot(app, player, self.vehicles[&id].1, hof.as_ref());
            result["id"] = json!(id.to_string());
            result["generation"] = json!(self.vehicles[&id].1.to_string());
            return Ok(result);
        }
        if matches!(operation, "hof.list" | "hof.get" | "hof.set") {
            validate_fields(
                args,
                if operation == "hof.list" {
                    &[]
                } else {
                    &["index"]
                },
            )?;
            let player = player_mut(app, id)?;
            let path = &player.vehicle.ty.def.path;
            let files =
                omsi_vehicle::hof::depot_files(path.parent().unwrap_or(std::path::Path::new("")));
            if operation == "hof.list" {
                return Ok(json!(files
                    .iter()
                    .enumerate()
                    .map(|(index, f)| json!({"index":index,
                    "file_name":f.file_name().unwrap_or_default().to_string_lossy(),
                    "selected":player.vehicle.host.hof.as_ref().is_some_and(|h| h.path == *f)}))
                    .collect::<Vec<_>>()));
            }
            let index = args
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("HOF index is required")?;
            let file = files
                .get(usize::try_from(index).map_err(|_| "invalid HOF index")?)
                .ok_or("HOF index is unavailable")?;
            let hof =
                omsi_vehicle::hof::Hof::load(file).map_err(|e| format!("HOF load failed: {e}"))?;
            if operation == "hof.set" {
                // A setter must not fail serializing a huge HOF after committing it.
                let receipt = json!({"index":index,"name":hof.name,
                    "file_name":hof.path.file_name().unwrap_or_default().to_string_lossy(),
                    "id":id.to_string(),"generation":self.vehicles[&id].1.to_string()});
                player.vehicle.host.hof = Some(Arc::new(hof));
                return Ok(receipt);
            }
            return Ok(hof_snapshot(&hof, path));
        }
        if matches!(operation, "set_variables" | "vehicle.set_variables") {
            if operation == "vehicle.set_variables" {
                validate_fields(args, &["values", "strings"])?;
            }
            let request = legacy_request(operation, args)?;
            validate_values(&request)?;
            if operation == "vehicle.set_variables"
                && request.values.is_empty()
                && request.strings.is_empty()
            {
                return Err("variable update is empty".into());
            }
            let vehicle = &mut player_mut(app, id)?.vehicle;
            for name in request.values.keys() {
                if vehicle.var(name).is_none() {
                    return Err(format!("unknown numeric variable: {name}"));
                }
            }
            let mut strings = Vec::new();
            for (name, value) in &request.strings {
                let index = vehicle
                    .ty
                    .program
                    .str_var(name)
                    .ok_or_else(|| format!("unknown string variable: {name}"))?
                    as usize;
                if index >= vehicle.state.str_vars.len() {
                    return Err("string variable is not instantiated".into());
                }
                strings.push((index, value));
            }
            for (name, value) in &request.values {
                vehicle.set_var(name, *value);
            }
            for (index, value) in strings {
                vehicle.state.str_vars[index] = value.clone();
            }
            return Ok(Value::Null);
        }
        if operation == "vehicle.trigger" {
            validate_fields(args, &["name"])?;
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty() && n.len() <= 256)
                .ok_or("trigger name is required")?;
            let vehicle = &mut player_mut(app, id)?.vehicle;
            if !vehicle.trigger(name) {
                return Err(format!("trigger is unavailable or failed: {name}"));
            }
            return Ok(Value::Null);
        }
        if operation == "vehicle.set_speed" {
            validate_fields(args, &["metres_per_second"])?;
            let speed = args
                .get("metres_per_second")
                .and_then(Value::as_f64)
                .ok_or("metres_per_second is required")? as f32;
            if !speed.is_finite() {
                return Err("speed must be finite".into());
            }
            if speed.abs() > 10_000.0 {
                return Err("speed must be within -10000..10000 metres per second".into());
            }
            player_mut(app, id)?.vehicle.set_speed(speed);
            return Ok(Value::Null);
        }
        {
            let player = player_mut(app, id)?;
            if matches!(
                operation,
                "vehicle.physics.parameters" | "vehicle.physics.configure"
            ) {
                return crate::plugin_api_physics::execute(&mut player.vehicle, operation, args);
            }
            if let Some(result) =
                crate::plugin_api_vehicle::execute(&mut player.vehicle, operation, args)
            {
                if operation == "vehicle.set_pose" && result.is_ok() {
                    // Rail lane indices and the old rail trail belong to the old pose.
                    // The normal rail frame will attach near the new position.
                    player.rail = None;
                    player.arm.reset();
                }
                return result;
            }
        }
        match operation {
            "texture.upload" => {
                validate_fields(args, &["section", "index", "width", "height", "format"])?
            }
            "texture.release" => validate_fields(args, &["section", "index", "lease", "resource"])?,
            "texture.invalidate" => validate_fields(args, &["path"])?,
            _ => {}
        }
        let request = legacy_request(operation, args)?;
        let renderer = app.renderer.as_ref().ok_or("renderer is not ready")?;
        let scene = app.scene.as_mut().ok_or("scene is not ready")?;
        let player = app
            .player
            .iter_mut()
            .chain(app.placed.iter_mut())
            .find(|p| p.uid == id)
            .ok_or("vehicle is no longer loaded")?;
        let texture_key = if matches!(
            operation,
            "script_texture" | "texture.upload" | "script_texture_release" | "texture.release"
        ) {
            let render = if request.section == 0 {
                &player.render
            } else {
                player
                    .trailer_renders
                    .get(request.section - 1)
                    .ok_or("vehicle section is not present")?
            };
            let texture = render
                .script_textures
                .get(request.index)
                .copied()
                .flatten()
                .ok_or("vehicle section does not declare this script texture")?;
            Some((id, self.vehicles[&id].1, texture))
        } else {
            None
        };
        match operation {
            "script_texture" | "texture.upload" => {
                let rgba = decode_bgra(&request, binary.to_vec())?;
                crate::scene::bridge_script_texture(
                    renderer,
                    scene,
                    player,
                    request.section,
                    request.index,
                    request.width,
                    request.height,
                    rgba,
                )?;
                self.next_texture_lease = self.next_texture_lease.wrapping_add(1).max(1);
                let key = texture_key.unwrap();
                self.texture_leases.insert(key, self.next_texture_lease);
                return Ok(
                    json!({"id":id.to_string(),"generation":key.1.to_string(),"section":request.section,
                    "index":request.index,"lease":self.next_texture_lease.to_string(),
                    "resource":format!("{}:{}:{}",key.0,key.1,key.2)}),
                );
            }
            "script_texture_release" | "texture.release" => {
                let key = texture_key.unwrap();
                if let Some(lease) = args.get("lease") {
                    let expected = lease
                        .as_u64()
                        .or_else(|| lease.as_str()?.parse().ok())
                        .ok_or("invalid texture lease")?;
                    if self.texture_leases.get(&key) != Some(&expected) {
                        return Err(
                            "texture was replaced or released by another API operation".into()
                        );
                    }
                }
                crate::scene::bridge_release_script_texture(
                    renderer,
                    scene,
                    player,
                    request.section,
                    request.index,
                )?;
                self.texture_leases.remove(&key);
            }
            "invalidate_texture" | "texture.invalidate" => {
                app.world
                    .as_ref()
                    .ok_or("map is not loaded")?
                    .bridge_refresh_texture(renderer, scene, player, &request.path)?;
            }
            _ => unreachable!(),
        }
        Ok(Value::Null)
    }
}

fn legacy_request(operation: &str, args: &Value) -> Result<Request, String> {
    let mut value = args.clone();
    let object = value
        .as_object_mut()
        .ok_or("API arguments must be an object")?;
    object.insert("protocol".into(), json!(1));
    object.insert("token".into(), json!(""));
    object.insert("request_id".into(), json!(0));
    object.insert("op".into(), json!(operation));
    // Native object identities are decimal strings (lossless in Lua); identity
    // validation has already happened before parsing the legacy payload fields.
    object.remove("id");
    object.remove("vehicle_id");
    object.remove("generation");
    serde_json::from_value(value).map_err(|e| format!("invalid operation arguments: {e}"))
}

fn validate_fields(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("API arguments must be an object")?
        .keys()
    {
        if !["id", "vehicle_id", "generation", "session_id"].contains(&key.as_str())
            && !allowed.contains(&key.as_str())
        {
            return Err(format!("unsupported argument: {key}"));
        }
    }
    Ok(())
}

fn player(app: &App, id: u64) -> Result<&Player, String> {
    app.player
        .iter()
        .chain(app.placed.iter())
        .find(|p| p.uid == id)
        .ok_or_else(|| "vehicle is no longer loaded".into())
}
fn player_mut(app: &mut App, id: u64) -> Result<&mut Player, String> {
    app.player
        .iter_mut()
        .chain(app.placed.iter_mut())
        .find(|p| p.uid == id)
        .ok_or_else(|| "vehicle is no longer loaded".into())
}

pub(crate) fn execute(
    app: &mut App,
    operation: &str,
    args: Value,
    binary: &[u8],
) -> Result<Value, String> {
    if binary.len() > crate::native_bridge::protocol::MAX_BINARY {
        return Err("binary payload exceeds 16 MiB".into());
    }
    if !args.is_object() {
        return Err("API arguments must be an object".into());
    }
    if serde_json::to_vec(&args).map_err(|e| e.to_string())?.len()
        > crate::native_bridge::protocol::MAX_JSON
    {
        return Err("API arguments exceed 2 MiB".into());
    }
    with_api_state(app, |state, app| {
        state.execute(app, operation, &args, binary)
    })?
}

/// Retain the recursion guard during dispatch, but restore it even when a native
/// handler unwinds. This is not a transaction: mutations made before a panic remain.
/// Resume the same panic after restoring ownership; never turn it into success.
fn with_api_state<T>(
    app: &mut App,
    operation: impl FnOnce(&mut ApiState, &mut App) -> T,
) -> Result<T, String> {
    let mut state = app.plugin_api.take().ok_or("recursive game API call")?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.refresh(app);
        operation(&mut state, app)
    }));
    app.plugin_api = Some(state);
    match result {
        Ok(value) => Ok(value),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

pub(crate) fn snapshot(app: &mut App) -> Value {
    with_api_state(app, |state, app| state.snapshot(app)).expect("game API state outside dispatch")
}

pub(crate) fn session(app: &mut App) -> String {
    with_api_state(app, |state, _| state.session.clone()).expect("game API state outside dispatch")
}

/// Bridge bookkeeping must forget receipts after Lua, an explicit release, or
/// vehicle unloading retired them, even when the owning connection stays open.
pub(crate) fn texture_receipt_is_current(app: &App, receipt: &Value) -> bool {
    let Some(resource) = receipt["resource"].as_str() else {
        return false;
    };
    let mut parts = resource.split(':');
    let (Some(id), Some(generation), Some(texture)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let (Ok(id), Ok(generation), Ok(texture)) = (id.parse(), generation.parse(), texture.parse())
    else {
        return false;
    };
    let Some(lease) = receipt["lease"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return false;
    };
    app.plugin_api
        .as_ref()
        .is_some_and(|state| state.texture_leases.get(&(id, generation, texture)) == Some(&lease))
}

/// Cheap lifecycle identity; does not construct a vehicle/script-state snapshot.
pub(crate) fn active_vehicle_identity(app: &mut App) -> Option<String> {
    with_api_state(app, |state, app| {
        let player = app.player.as_ref()?;
        let (_, generation) = state.vehicles.get(&player.uid)?;
        Some(format!("{}:{}:{}", state.session, player.uid, generation))
    })
    .expect("game API state outside dispatch")
}

fn snapshot_value(
    app: &App,
    session: &str,
    generation: u64,
    sequence: u64,
    hof: Option<&Value>,
) -> Value {
    let vehicle = app
        .player
        .as_ref()
        .map(|p| vehicle_snapshot(app, p, generation, hof));
    json!({"session_id":session,"sequence":sequence,
        "capabilities":["variables","scenery_variables_source","script_texture","script_texture_release","invalidate_texture","hof","timetable_basic","set_clock","native_api","traffic_light_phase_precondition"],
        "native_operations":NATIVE_OPERATIONS,
        "map_name":app.world.as_ref().map(|w|w.global.name.as_str()).unwrap_or(""),
        "game_root":app.args.root.to_string_lossy(),
        "clock":clock_snapshot(app),"vehicle":vehicle})
}

fn clock_snapshot(app: &App) -> Value {
    let (day, month) = app.clock.day_month();
    let seconds = app.clock.time.max(0.0) as u32;
    json!({"hour":seconds/3600,"minute":seconds/60%60,"second":app.clock.time.rem_euclid(60.0),
        "day":day,"month":month,"year":app.clock.year,"day_of_year":app.clock.day_of_year,
        "service_seconds":app.clock.time})
}

fn vehicle_snapshot(app: &App, p: &Player, generation: u64, hof: Option<&Value>) -> Value {
    let brightness = omsi_sim::Daylight::compute(&app.clock, app.envir.as_ref()).brightness;
    let v = &p.vehicle;
    let ((tx, ty), (x, z)) = omsi_map::world_to_tile_local(v.position.x, v.position.y);
    let tile = app
        .world
        .as_ref()
        .and_then(|w| w.global.tiles.iter().find(|t| t.x == tx && t.y == ty))
        .map(|t| t.index as i64)
        .unwrap_or(-1);
    let mut variables: BTreeMap<_, _> =
        v.ty.program
            .var_names
            .iter()
            .zip(&v.state.vars)
            .filter(|(_, value)| value.is_finite())
            .map(|(name, value)| (name.clone(), *value))
            .collect();
    // Engine-injected model variables follow the compiled script variables.
    for (index, value) in v
        .state
        .vars
        .iter()
        .enumerate()
        .skip(v.ty.program.var_names.len())
    {
        if value.is_finite() {
            if let Some(name) = v.var_name(index) {
                variables.insert(name.to_string(), *value);
            }
        }
    }
    let strings: BTreeMap<_, _> =
        v.ty.program
            .str_var_names
            .iter()
            .zip(&v.state.str_vars)
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
    let script_textures: Vec<_> =
        if let (Some(renderer), Some(scene)) = (app.renderer.as_ref(), app.scene.as_ref()) {
            std::iter::once(&p.render)
                .chain(p.trailer_renders.iter())
                .enumerate()
                .flat_map(|(section, render)| {
                    render.script_textures.iter().enumerate()
                    .filter_map(move |(index, texture)| {
                        let (width, height, _) = renderer.texture_levels(scene, (*texture)?)?;
                        Some(json!({"section":section,"index":index,"width":width,"height":height}))
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
    json!({"id":p.uid,"generation":generation,"file_name":v.ty.def.path.to_string_lossy(),
            "friendly_name":format!("{} {}",v.ty.def.manufacturer,v.ty.def.type_name).trim(),
            "tile_index":tile,"tile_x":tx,"tile_y":ty,"position":{"x":x,"y":v.position.z,"z":z},
            "heading":v.heading.to_radians(),"brightness":brightness,
            "head_position":{"x":p.head.x,"y":p.head.z,"z":p.head.y},
            "head_velocity":{"x":p.head_vel.x,"y":p.head_vel.z,"z":p.head_vel.y},
            "world_position":{"east":v.position.x,"north":v.position.y,"up":v.position.z},
            "pitch":v.pitch,"bank":v.bank,"speed_metres_per_second":v.physics.speed,
            "controls":{"throttle":v.physics.controls.throttle,"brake":v.physics.controls.brake,
                "clutch":v.physics.controls.clutch,"steering":v.physics.controls.steering},
            "triggers":v.ty.program.trigger_names(),
            "variables":variables,"strings":strings,"script_textures":script_textures,
            "hof":hof,"schedule":if app.player.as_ref().is_some_and(|active|active.uid==p.uid) {
                schedule_snapshot(app)
            } else { Value::Null }})
}

fn hof_snapshot(h: &omsi_vehicle::hof::Hof, vehicle_path: &std::path::Path) -> Value {
    let files =
        omsi_vehicle::hof::depot_files(vehicle_path.parent().unwrap_or(std::path::Path::new("")));
    let index = files
        .iter()
        .position(|f| f == &h.path)
        .map(|n| n as i64)
        .unwrap_or(-1);
    let trips: Vec<_> = h
        .info_trips
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
        "code":t.code.trim().parse::<i32>().ok(),"code_text":t.code,"name":t.name,
        "target":t.route.trim().parse::<i32>().ok(),"route":t.route,"line":t.line,"extra":t.extra,
        "busstops":h.info_busstop_lists.get(i).cloned().unwrap_or_default()})
        })
        .collect();
    let stops: Vec<_> = h
        .bus_stops
        .iter()
        .map(|s| json!({"ident":s.ident,"strings":s.strings}))
        .collect();
    let termini: Vec<_> = h
        .termini
        .iter()
        .map(|t| {
            json!({"code":t.code,"texture_id":t.texture_id,
        "terminus_stop":t.terminus_stop,"all_exit":t.all_exit,"strings":t.strings})
        })
        .collect();
    json!({"index":index,"file_name":h.path.file_name().unwrap_or_default().to_string_lossy(),
        "name":h.name,"service_trip":h.service_trip,"global_strings":h.global_strings,
        "string_count_terminus":h.string_count_terminus,"string_count_busstop":h.string_count_busstop,
        "termini":termini,"trips":trips,"busstops":stops,"info_busstops":h.info_busstops})
}

fn schedule_snapshot(app: &App) -> Value {
    let mut out = json!({"active":false,"line":-1,"tour":-1,"trip":-1,"tour_entry":-1,
        "delay":0.0,"next_stop_arrival":-1.0,"next_stop_time_to_depart":-1.0,
        "previous_stop_distance":-1.0,"next_stop_distance":-1.0});
    let (Some(duty), Some(player)) = (&app.duty, &app.player) else {
        return out;
    };
    let Some(trip) = duty.trips.get(duty.trip_index) else {
        return out;
    };
    out["active"] = json!(true);
    out["delay"] = json!(duty.delay(app.clock.time));
    if let Some(data) = app.schedule.as_ref().map(|s| &s.data) {
        if let Some(li) = data
            .lines
            .iter()
            .position(|l| l.name.eq_ignore_ascii_case(&duty.line))
        {
            out["line"] = json!(li);
            if let Some(ti) = data.lines[li]
                .tours
                .iter()
                .position(|t| t.number == duty.tour)
            {
                out["tour"] = json!(ti);
                if let Some(ei) = data.lines[li].tours[ti].trips.iter().position(|t| {
                    t.trip.eq_ignore_ascii_case(&trip.name)
                        && ((t.departure as f64 * 60.0 - trip.departure).rem_euclid(86400.0))
                            .min((trip.departure - t.departure as f64 * 60.0).rem_euclid(86400.0))
                            < 1.0
                }) {
                    out["tour_entry"] = json!(ei);
                }
            }
        }
        if let Some(i) = data
            .trips
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(&trip.name))
        {
            out["trip"] = json!(i);
        }
    }
    let center = (trip.departure + trip.end) * 0.5;
    let now = [
        app.clock.time - 86400.0,
        app.clock.time,
        app.clock.time + 86400.0,
    ]
    .into_iter()
    .min_by(|a, b| (a - center).abs().total_cmp(&(b - center).abs()))
    .unwrap();
    if let Some(stop) = trip.stops.get(duty.next_stop) {
        out["next_stop_arrival"] = json!(stop.arr);
        out["next_stop_time_to_depart"] = json!(stop.dep - now);
        if let Some(pos) = stop.position {
            out["next_stop_distance"] = json!((pos - player.vehicle.position).length());
        }
    }
    if let Some(stop) = duty
        .next_stop
        .checked_sub(1)
        .and_then(|i| trip.stops.get(i))
    {
        if let Some(pos) = stop.position {
            out["previous_stop_distance"] = json!((pos - player.vehicle.position).length());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn native_dispatch_unwind_restores_state_and_resumes_without_rollback() {
        let mut app = crate::new_app(
            crate::Args::parse_from(["openomsi", "--root", "."]),
            crate::settings::Settings::default(),
        );
        let before = session(&mut app);
        app.paused = false;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_api_state(&mut app, |state, app| -> () {
                assert!(execute(app, "clock.get", json!({}), &[])
                    .unwrap_err()
                    .contains("recursive"));
                state.sequence = 42;
                app.paused = true;
                panic!("fixture native operation panic");
            })
            .unwrap();
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"fixture native operation panic")
        );
        assert_eq!(app.plugin_api.as_ref().unwrap().sequence, 42);
        assert!(
            app.paused,
            "restoring dispatch ownership must not pretend to roll back game mutations"
        );
        assert_eq!(session(&mut app), before);
        assert!(execute(&mut app, "clock.get", json!({}), &[]).is_ok());
    }
}
