//! Main-thread native access to live scenery, AI vehicles and traffic light programs.
//! Handles describe native loaded instances, not OMSI's unrelated memory-array indexes.

use crate::{App, scene::World};
use omsi_script::{Program, State};
use omsi_sim::scenery::SceneryInstance;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(crate) fn execute(app: &mut App, op: &str, args: &Value) -> Option<Result<Value, String>> {
    let traffic_write = matches!(
        op,
        "traffic.set"
            | "traffic.trigger"
            | "traffic.behavior.set"
            | "traffic.remove"
            | "traffic.spawn_on_path"
            | "traffic.service.hold"
            | "traffic.service.set_departure"
            | "traffic.lights.set"
            | "traffic.paths.set"
    );
    if traffic_write && app.traffic.as_ref().is_some_and(|t| t.api_is_mirror()) {
        return Some(Err(
            "network-mirrored traffic is controlled by its host".into()
        ));
    }
    Some(match op {
        "scenery.list" => scenery_list(app, args),
        "scenery.get" | "scenery.set" | "scenery.trigger" => scenery_access(app, op, args),
        "scenery.placements.list" => placement_list(app, args),
        "scenery.placements.get" => placement_get(app, args),
        "scenery.placements.set" => placement_set(app, args),
        "traffic.list" => traffic_list(app, args),
        "traffic.get" | "traffic.set" | "traffic.trigger" => traffic_access(app, op, args),
        "traffic.behavior.get" | "traffic.behavior.set" => traffic_behavior(app, op, args),
        "traffic.spawn_on_path" => traffic_spawn(app, args),
        "traffic.remove" => traffic_remove(app, args),
        "traffic.service.get" | "traffic.service.hold" | "traffic.service.set_departure" => {
            traffic_service(app, op, args)
        }
        "traffic.lights.list" => light_list(app, args),
        "traffic.lights.get" | "traffic.lights.set" => light_access(app, op, args),
        "traffic.paths.list" => path_list(app, args),
        "traffic.paths.get" | "traffic.paths.set" => path_access(app, op, args),
        _ => return None,
    })
}

fn text<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} must be a string"))
}

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported argument {key}"));
        }
    }
    Ok(())
}

fn handle(value: &str, prefix: &str) -> Result<u64, String> {
    value
        .strip_prefix(prefix)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("invalid {prefix} handle"))
}

fn page(args: &Value, prefix: &str) -> Result<(u64, usize), String> {
    let after = args
        .get("after_id")
        .map(|v| v.as_str().ok_or("after_id must be a string"))
        .transpose()?
        .map(|s| handle(s, prefix))
        .transpose()?
        .unwrap_or(0);
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().ok_or("limit must be an integer"))
        .transpose()?
        .unwrap_or(128);
    if !(1..=512).contains(&limit) {
        return Err("limit must be between 1 and 512".into());
    }
    Ok((after, limit as usize))
}

fn paged(mut rows: Vec<(u64, Value)>, limit: usize, prefix: &str) -> Value {
    rows.sort_by_key(|(id, _)| *id);
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = more.then(|| format!("{prefix}{}", rows.last().unwrap().0));
    json!({"items": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(), "next_after_id":next})
}

fn tile_index(world: &World, tile: (i32, i32)) -> Option<usize> {
    world
        .global
        .tiles
        .iter()
        .find(|t| (t.x, t.y) == tile)
        .map(|t| t.index)
}

fn scenery_list(app: &App, args: &Value) -> Result<Value, String> {
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let (after, limit) = page(args, "scenery:")?;
    let filter_tile = args
        .get("tile_index")
        .map(|v| v.as_u64().ok_or("tile_index must be an integer"))
        .transpose()?;
    let filter_path = args
        .get("path_contains")
        .map(|v| v.as_str().ok_or("path_contains must be a string"))
        .transpose()?
        .map(|s| s.replace('\\', "/").to_lowercase());
    let mut rows = Vec::new();
    let matches = |id: u64, tile, path: &std::path::Path| {
        id > after
            && filter_tile.is_none_or(|index| tile_index(world, tile) == Some(index as usize))
            && filter_path.as_ref().is_none_or(|needle| {
                path.to_string_lossy()
                    .replace('\\', "/")
                    .to_lowercase()
                    .contains(needle)
            })
    };
    for object in world.scripted.lock().iter() {
        if matches(object.api_id, object.tile, &object.ty.sco.path) {
            let (controller, light_index) = world.bridge_light_binding(object);
            rows.push((object.api_id, json!({
                "id":format!("scenery:{}",object.api_id),"kind":"scripted",
                "map_id":object.map_id.to_string(),"tile_index":tile_index(world, object.tile),
                "tile":[object.tile.0,object.tile.1],"file_name":object.ty.sco.path.to_string_lossy(),
                "position":[object.pos.x,object.pos.y,object.pos.z],"has_script":true,
                "light_controller":controller,"light_index":light_index,
            })));
        }
    }
    for object in world.light_objects.lock().iter() {
        if matches(object.api_id, object.tile, &object.type_path) {
            rows.push((object.api_id, json!({
                "id":format!("scenery:{}",object.api_id),"kind":"traffic_light",
                "map_id":object.map_id.to_string(),"tile_index":tile_index(world, object.tile),
                "tile":[object.tile.0,object.tile.1],"file_name":object.type_path.to_string_lossy(),
                "position":[object.pos.x,object.pos.y,object.pos.z],"has_script":object.script.is_some(),
                "parent_map_id":object.parent.to_string(),"light_index":object.index,
            })));
        }
    }
    Ok(paged(rows, limit, "scenery:"))
}

fn placement_row(world: &World, map_id: i64, object: &crate::scene::EditObject) -> Value {
    let edit = world
        .object_edits
        .lock()
        .get(&map_id)
        .copied()
        .unwrap_or_default();
    let position = object.pos + edit.moved;
    let source = object.native_origin.unwrap_or(crate::tiles::Pose {
        pos: object.pos,
        rot: object.xf,
    });
    json!({"id":format!("placement:{}",object.api_id),"map_id":map_id.to_string(),
        "tile_index":tile_index(world,object.tile),"tile":[object.tile.0,object.tile.1],
        "file_name":object.sco.to_string_lossy(),"position":[position.x,position.y,position.z],
        "source_position":[source.pos.x,source.pos.y,source.pos.z],"source_transform":source.rot.to_cols_array(),
        "heading_deg":crate::tiles::Pose {pos:position,rot:object.xf}.heading()+edit.turned,
        "transform":(glam::Mat4::from_rotation_z(-edit.turned.to_radians() as f32)*object.xf).to_cols_array(),
        "runtime_relocated":object.native_origin.is_some(),
        "heading_delta_deg":edit.turned,"deleted":edit.deleted,
    })
}

fn placement_list(app: &App, args: &Value) -> Result<Value, String> {
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let (after, limit) = page(args, "placement:")?;
    let tile = args
        .get("tile_index")
        .map(|v| v.as_u64().ok_or("tile_index must be an integer"))
        .transpose()?;
    let objects: Vec<_> = world
        .edit_objects
        .lock()
        .iter()
        .filter(|(_, o)| {
            o.api_id > after && tile.is_none_or(|t| tile_index(world, o.tile) == Some(t as usize))
        })
        .map(|(id, o)| (*id, o.clone()))
        .collect();
    Ok(paged(
        objects
            .iter()
            .map(|(id, o)| (o.api_id, placement_row(world, *id, o)))
            .collect(),
        limit,
        "placement:",
    ))
}

fn placement_get(app: &App, args: &Value) -> Result<Value, String> {
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let id = handle(text(args, "id")?, "placement:")?;
    let (map_id, object) = world
        .edit_objects
        .lock()
        .iter()
        .find(|(_, o)| o.api_id == id)
        .map(|(map_id, o)| (*map_id, o.clone()))
        .ok_or("placement handle is stale or not loaded")?;
    let mut row = placement_row(world, map_id, &object);
    let mut scripts: Vec<_> = world
        .scripted
        .lock()
        .iter()
        .filter(|o| o.map_id == map_id && o.tile == object.tile)
        .map(|o| format!("scenery:{}", o.api_id))
        .collect();
    scripts.extend(
        world
            .light_objects
            .lock()
            .iter()
            .filter(|o| o.map_id == map_id && o.tile == object.tile)
            .map(|o| format!("scenery:{}", o.api_id)),
    );
    row["script_handles"] = json!(scripts);
    Ok(row)
}

fn placement_set(app: &mut App, args: &Value) -> Result<Value, String> {
    keys(args, &["session_id", "id", "position", "heading_deg"])?;
    let id = handle(text(args, "id")?, "placement:")?;
    let position = args
        .get("position")
        .map(|v| {
            let a = v
                .as_array()
                .filter(|a| a.len() == 3)
                .ok_or("position must contain exactly three numbers")?;
            let mut p = [0.0; 3];
            for (to, value) in p.iter_mut().zip(a) {
                *to = value.as_f64().ok_or("position must contain numbers")?;
            }
            Ok::<_, String>(glam::DVec3::from(p))
        })
        .transpose()?;
    let heading = args
        .get("heading_deg")
        .map(|v| v.as_f64().ok_or("heading_deg must be a number"))
        .transpose()?;
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    if app.lan.is_some() {
        return Err("native scenery relocation is unavailable during network play".into());
    }
    let renderer = app.renderer.as_ref().ok_or("renderer is not ready")?;
    let scene = app.scene.as_mut().ok_or("scene is not ready")?;
    let pose = world.bridge_set_placement(renderer, scene, id, position, heading)?;
    Ok(
        json!({"id":format!("placement:{id}"),"position":pose.pos.to_array(),
        "heading_deg":pose.heading(),"lifetime":"loaded_instance"}),
    )
}

fn script_snapshot(program: &Program, state: &State) -> Value {
    let values: Map<_, _> = program
        .var_names
        .iter()
        .zip(&state.vars)
        .map(|(name, value)| (name.clone(), json!(value)))
        .collect();
    let strings: Map<_, _> = program
        .str_var_names
        .iter()
        .zip(&state.str_vars)
        .map(|(name, value)| (name.clone(), json!(value)))
        .collect();
    json!({"values":values,"strings":strings,"triggers":program.trigger_names()})
}

struct ScriptUpdate {
    values: Vec<(String, f32)>,
    strings: Vec<(String, String)>,
}

impl ScriptUpdate {
    fn parse(args: &Value) -> Result<Self, String> {
        keys(args, &["session_id", "id", "values", "strings"])?;
        let mut update = Self {
            values: Vec::new(),
            strings: Vec::new(),
        };
        for (key, string_values) in [("values", false), ("strings", true)] {
            let Some(value) = args.get(key) else { continue };
            let entries = value
                .as_object()
                .ok_or_else(|| format!("{key} must be an object"))?;
            if entries.len() > 1024 {
                return Err(format!("{key} has more than 1024 entries"));
            }
            let mut names = HashSet::new();
            for (name, value) in entries {
                if name.is_empty() || name.len() > 256 || !names.insert(name.to_ascii_lowercase()) {
                    return Err(format!("invalid or duplicate variable {name:?}"));
                }
                if string_values {
                    let value = value
                        .as_str()
                        .ok_or_else(|| format!("{name} must be a string"))?;
                    if value.len() > 16384 {
                        return Err(format!("string {name} is too long"));
                    }
                    update.strings.push((name.clone(), value.to_string()));
                } else {
                    let value = value
                        .as_f64()
                        .ok_or_else(|| format!("{name} must be a number"))?
                        as f32;
                    if !value.is_finite() {
                        return Err(format!("{name} must be a finite float"));
                    }
                    update.values.push((name.clone(), value));
                }
            }
        }
        if update.values.is_empty() && update.strings.is_empty() {
            return Err("set needs a nonempty values or strings object".into());
        }
        if update.strings.iter().map(|(_, s)| s.len()).sum::<usize>() > 1024 * 1024 {
            return Err("combined strings exceed 1 MiB".into());
        }
        Ok(update)
    }

    fn validate(
        &self,
        numeric_exists: impl Fn(&str) -> bool,
        program: &Program,
    ) -> Result<(), String> {
        for (name, _) in &self.values {
            if !numeric_exists(name) {
                return Err(format!("unknown numeric variable {name}"));
            }
        }
        for (name, _) in &self.strings {
            if program.str_var(name).is_none() {
                return Err(format!("unknown string variable {name}"));
            }
        }
        Ok(())
    }

    fn strings_apply(&self, program: &Program, state: &mut State) {
        for (name, value) in &self.strings {
            state.str_vars[program.str_var(name).unwrap() as usize].clone_from(value);
        }
    }
}

fn scenery_script_access(
    inst: &mut SceneryInstance,
    op: &str,
    args: &Value,
) -> Result<Value, String> {
    if op == "scenery.set" {
        let update = ScriptUpdate::parse(args)?;
        update.validate(|name| inst.program.var(name).is_some(), &inst.program)?;
        for (name, value) in &update.values {
            inst.set_var(name, *value);
        }
        update.strings_apply(&inst.program, &mut inst.state);
        if !update.strings.is_empty() {
            inst.set_var("Refresh_Strings", 1.0);
        }
        return Ok(
            json!({"updated_values":update.values.len(),"updated_strings":update.strings.len()}),
        );
    } else if op == "scenery.trigger" {
        let name = text(args, "name")?;
        if inst.program.trigger(name).is_none() {
            return Err(format!("unknown scenery trigger {name}"));
        }
        if !inst.trigger(name) {
            return Err(format!("scenery trigger {name} failed"));
        }
        return Ok(json!({"triggered":true}));
    }
    Ok(script_snapshot(&inst.program, &inst.state))
}

fn scenery_access(app: &App, op: &str, args: &Value) -> Result<Value, String> {
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let id = handle(text(args, "id")?, "scenery:")?;
    if let Some(object) = world
        .scripted
        .lock()
        .iter_mut()
        .find(|object| object.api_id == id)
    {
        return scenery_script_access(&mut object.inst, op, args);
    }
    let lights = world.light_objects.lock();
    let object = lights
        .iter()
        .find(|object| object.api_id == id)
        .ok_or("scenery handle is stale or not loaded")?;
    let script = object
        .script
        .as_ref()
        .ok_or("this light object has no script state")?;
    let result = scenery_script_access(&mut script.lock(), op, args);
    result
}

fn traffic_row(car: &crate::traffic::AiCar) -> Value {
    json!({"id":format!("traffic:{}",car.api_id),"simulation_id":car.id.to_string(),"file_name":car.vehicle.ty.def.path.to_string_lossy(),
        "position":[car.vehicle.position.x,car.vehicle.position.y,car.vehicle.position.z],
        "heading_deg":car.vehicle.heading,"speed_mps":car.state.speed,"lane":car.state.lane,
        "lane_distance_m":car.state.s,"is_bus":car.is_bus(),"is_rail":car.is_rail(),
        "braking":car.state.braking,"blinker":car.state.blinker,"gone":car.gone,
        "route_length":car.state.route.len(),"route_index":car.state.route_index,
        "max_speed_kmh":car.state.max_speed_kmh,"accel_mps2":car.state.accel,"decel_mps2":car.state.decel,
        "lat_accel_mps2":car.state.lat_accel,"headway_s":car.state.headway,"min_gap_m":car.state.min_gap,
        "desired_speed_factor":car.state.desire,
    })
}

fn traffic_list(app: &App, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_ref().ok_or("traffic is not loaded")?;
    let (after, limit) = page(args, "traffic:")?;
    let buses = args
        .get("buses_only")
        .map(|v| v.as_bool().ok_or("buses_only must be a boolean"))
        .transpose()?
        .unwrap_or(false);
    let rows = traffic
        .cars
        .iter()
        .filter(|c| c.api_id > after && (!buses || c.is_bus()))
        .map(|c| (c.api_id, traffic_row(c)))
        .collect();
    Ok(paged(rows, limit, "traffic:"))
}

fn traffic_access(app: &mut App, op: &str, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let id = handle(text(args, "id")?, "traffic:")?;
    let car = traffic
        .cars
        .iter_mut()
        .find(|car| car.api_id == id)
        .ok_or("traffic handle is stale or not active")?;
    if op == "traffic.set" {
        let update = ScriptUpdate::parse(args)?;
        update.validate(
            |name| car.vehicle.var(name).is_some(),
            &car.vehicle.ty.program,
        )?;
        for (name, value) in &update.values {
            car.vehicle.set_var(name, *value);
        }
        update.strings_apply(&car.vehicle.ty.program, &mut car.vehicle.state);
        return Ok(
            json!({"id":format!("traffic:{id}"),"updated_values":update.values.len(),"updated_strings":update.strings.len()}),
        );
    } else if op == "traffic.trigger" {
        let name = text(args, "name")?;
        if car.vehicle.ty.program.trigger(name).is_none() {
            return Err(format!("unknown traffic vehicle trigger {name}"));
        }
        if !car.vehicle.trigger(name) {
            return Err(format!("traffic vehicle trigger {name} failed"));
        }
        return Ok(json!({"id":format!("traffic:{id}"),"triggered":true}));
    }
    let mut result = traffic_row(car);
    result["route"] = json!(car.state.route);
    let mut script = script_snapshot(&car.vehicle.ty.program, &car.vehicle.state);
    // Vehicle engine variables can follow the compiled program's declarations.
    let values = script["values"].as_object_mut().unwrap();
    for (index, value) in car.vehicle.state.vars.iter().enumerate() {
        if let Some(name) = car.vehicle.var_name(index) {
            values.insert(name.to_string(), json!(value));
        }
    }
    result["script"] = script;
    Ok(result)
}

fn traffic_behavior(app: &mut App, op: &str, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let id = handle(text(args, "id")?, "traffic:")?;
    let car = traffic
        .cars
        .iter_mut()
        .find(|c| c.api_id == id)
        .ok_or("traffic handle is stale or not active")?;
    if op == "traffic.behavior.set" {
        behavior_apply(&mut car.state, args)?;
    }
    Ok(traffic_row(car))
}

fn behavior_apply(state: &mut omsi_sim::traffic::AiState, args: &Value) -> Result<(), String> {
    keys(args, &["session_id", "id", "values"])?;
    let values = args
        .get("values")
        .and_then(Value::as_object)
        .filter(|v| !v.is_empty())
        .ok_or("values must be a nonempty object")?;
    let mut updates = Vec::new();
    for (name, value) in values {
        let (min, max) = match name.as_str() {
            "max_speed_kmh" => (0.0, 2000.0),
            "accel_mps2" | "decel_mps2" | "lat_accel_mps2" => (0.01, 100.0),
            "headway_s" => (0.1, 60.0),
            "min_gap_m" => (0.1, 100.0),
            "desired_speed_factor" => (0.01, 10.0),
            _ => return Err(format!("unsupported driver property {name}")),
        };
        updates.push((name.as_str(), number(value, name, min, max)?));
    }
    for (name, value) in updates {
        match name {
            "max_speed_kmh" => state.max_speed_kmh = value,
            "accel_mps2" => state.accel = value,
            "decel_mps2" => state.decel = value,
            "lat_accel_mps2" => state.lat_accel = value,
            "headway_s" => state.headway = value,
            "min_gap_m" => state.min_gap = value,
            "desired_speed_factor" => state.desire = value,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn traffic_spawn(app: &mut App, args: &Value) -> Result<Value, String> {
    keys(
        args,
        &[
            "session_id",
            "generation",
            "path_id",
            "distance_m",
            "file_name",
            "paint_scheme",
            "initial_speed_mps",
        ],
    )?;
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    if text(args, "generation")? != path_generation(traffic) {
        return Err("stale path generation".into());
    }
    let lane = text(args, "path_id")?
        .strip_prefix("path:")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or("invalid path handle")?;
    let path = traffic.net.lanes.get(lane).ok_or("path is not loaded")?;
    let distance = number(
        args.get("distance_m").ok_or("distance_m is required")?,
        "distance_m",
        0.0,
        path.length(),
    )?;
    let speed = args
        .get("initial_speed_mps")
        .map(|v| number(v, "initial_speed_mps", 0.0, 600.0))
        .transpose()?
        .unwrap_or(0.0);
    let relative = vehicle_asset_path(text(args, "file_name")?)?;
    let path = omsi_cfg::resolve_path(&world.root, &relative.to_string_lossy());
    let root = world.root.canonicalize().map_err(|e| e.to_string())?;
    let canonical = path.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(&root) {
        return Err("vehicle file resolves outside the map's content root".into());
    }
    let renderer = app.renderer.as_ref().ok_or("renderer is not ready")?;
    let scene = app.scene.as_mut().ok_or("scene is not ready")?;
    let ty = traffic
        .loaded_type(&path)
        .or_else(|| {
            traffic
                .cars
                .iter()
                .find(|c| c.vehicle.ty.def.path == path)
                .map(|c| c.vehicle.ty.clone())
        })
        .or_else(|| {
            app.player
                .as_ref()
                .filter(|p| p.vehicle.ty.def.path == path)
                .map(|p| p.vehicle.ty.clone())
        });
    let ty = match ty {
        Some(ty) => ty,
        None => std::sync::Arc::new(
            omsi_sim::VehicleType::load_ai(&world.root, &path).map_err(|e| e.to_string())?,
        ),
    };
    let scheme = args
        .get("paint_scheme")
        .map(|v| {
            if v.is_null() {
                Ok(None)
            } else {
                v.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .map(Some)
                    .ok_or("paint_scheme must be an index or null")
            }
        })
        .transpose()?;
    if let Some(weather) = app.weather.as_ref() {
        traffic.set_weather(weather, app.wetness);
    }
    let id = traffic.api_spawn_car(world, renderer, scene, ty, lane, distance, scheme, speed)?;
    let car = traffic
        .cars
        .iter()
        .find(|c| c.api_id == id)
        .ok_or("created vehicle vanished")?;
    Ok(traffic_row(car))
}

fn vehicle_asset_path(value: &str) -> Result<std::path::PathBuf, String> {
    let path = std::path::PathBuf::from(value.replace('\\', "/"));
    if value.is_empty()
        || value.len() > 1024
        || path
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err(
            "vehicle file must be a relative content path without parent components".into(),
        );
    }
    if !path.components().next().is_some_and(|p| {
        p.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("vehicles")
    }) {
        return Err("vehicle file must be under Vehicles".into());
    }
    if !path.extension().is_some_and(|x| {
        x.to_string_lossy().eq_ignore_ascii_case("bus")
            || x.to_string_lossy().eq_ignore_ascii_case("ovh")
    }) {
        return Err("vehicle file must be a .bus or .ovh".into());
    }
    Ok(path)
}

fn traffic_remove(app: &mut App, args: &Value) -> Result<Value, String> {
    keys(args, &["session_id", "id"])?;
    let id = handle(text(args, "id")?, "traffic:")?;
    let world = app.world.as_ref().ok_or("map is not loaded")?;
    let renderer = app.renderer.as_ref().ok_or("renderer is not ready")?;
    let scene = app.scene.as_mut().ok_or("scene is not ready")?;
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let car = traffic
        .cars
        .iter()
        .find(|c| c.api_id == id)
        .ok_or("traffic handle is stale or not active")?;
    let simulation_id = car.id;
    // Everything that may fail is checked before detaching passengers and ownership.
    if let Some(humans) = app.humans.as_mut() {
        humans.evict(crate::humans::BusId::Ai(simulation_id), world);
    }
    if let Some(schedule) = app.schedule.as_mut() {
        schedule.forget_vehicle(simulation_id);
    }
    if !traffic.remove_car(world, renderer, scene, simulation_id) {
        return Err("vehicle removal failed".into());
    }
    Ok(json!({"removed":format!("traffic:{id}"),"simulation_id":simulation_id.to_string()}))
}

fn traffic_service(app: &mut App, op: &str, args: &Value) -> Result<Value, String> {
    if op == "traffic.service.get" {
        keys(args, &["session_id", "id"])?;
    }
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let id = handle(text(args, "id")?, "traffic:")?;
    let car = traffic
        .cars
        .iter_mut()
        .find(|c| c.api_id == id)
        .ok_or("traffic handle is stale or not active")?;
    let schedule = app
        .schedule
        .as_ref()
        .and_then(|s| s.vehicle_service(car.id, !car.gone));
    let Some(bus) = car.bus.as_mut() else {
        if op == "traffic.service.get" {
            return Ok(json!({"id":format!("traffic:{id}"),"phase":null,"schedule":schedule}));
        }
        return Err("vehicle has no native bus service".into());
    };
    service_apply(bus, op, args)?;
    if op != "traffic.service.get" {
        return Ok(json!({"id":format!("traffic:{id}"),"updated":true,
            "boarding_seconds":bus.boarding,"leave_at":bus.leave_at}));
    }
    Ok(
        json!({"id":format!("traffic:{id}"),"phase":format!("{:?}",bus.phase).to_lowercase(),"schedule":schedule,
            "phase_seconds":bus.phase_t,"boarding_seconds":bus.boarding,"leave_at":bus.leave_at,
            "delay_seconds":bus.delay,"layover":bus.layover,"route_open":bus.route_open,"terminus":bus.terminus,
            "stops":bus.stops.iter().map(|stop|json!({"route_index":stop.ri,"distance_m":stop.s,"bay_m":stop.bay,
                "departure_seconds":stop.depart,"object_id":stop.id.to_string()})).collect::<Vec<_>>()
        }),
    )
}

fn service_apply(
    bus: &mut crate::bus_service::BusService,
    op: &str,
    args: &Value,
) -> Result<(), String> {
    if op == "traffic.service.hold" {
        keys(args, &["session_id", "id", "seconds"])?;
        let seconds = number(
            args.get("seconds").ok_or("seconds is required")?,
            "seconds",
            0.0,
            3600.0,
        )?;
        if bus.phase != crate::bus_service::Phase::Boarding {
            return Err("bus is not currently boarding".into());
        }
        bus.hold(seconds);
    } else if op == "traffic.service.set_departure" {
        keys(args, &["session_id", "id", "departure_seconds"])?;
        let departure = args
            .get("departure_seconds")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0 && *v < 172800.0)
            .ok_or("departure_seconds must be within the current service day or next day")?;
        if !matches!(
            bus.phase,
            crate::bus_service::Phase::Boarding | crate::bus_service::Phase::Waiting
        ) {
            return Err("departure can only change while the bus is boarding or waiting".into());
        }
        let stop = bus.stops.front_mut().ok_or("service has no current stop")?;
        stop.depart = departure;
        bus.leave_at = departure;
    }
    Ok(())
}

fn light_row(object: i64, ctl: &omsi_sim::traffic::TrafficLightController) -> Value {
    json!({"object_id":object.to_string(),"time":ctl.time,"held":ctl.held,"running":ctl.running,
        "names":ctl.names,
        "cycle":ctl.cycle,"cycle_length":ctl.cycle_len(),"offset":ctl.offset,"requests":ctl.request,
        "states":(0..ctl.lights.len()).map(|i|ctl.state(i)).collect::<Vec<_>>(),
        "phase_indices":(0..ctl.lights.len()).map(|i|ctl.phase_index(i)).collect::<Vec<_>>(),
        "allows_go":(0..ctl.lights.len()).map(|i|omsi_sim::traffic::TrafficLightController::allows_go(ctl.state(i))).collect::<Vec<_>>(),
        "lights":ctl.lights.iter().map(|phases| phases.iter().map(|(state,duration)|
            json!({"state":state,"duration":duration})).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "approach":ctl.approach,"stops":ctl.stops.iter().map(|s|
            json!({"light":s.light,"time":s.time,"if_request":s.if_request,"jump_to":s.jump_to})).collect::<Vec<_>>()})
}

fn light_list(app: &App, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_ref().ok_or("traffic is not loaded")?;
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().ok_or("limit must be an integer"))
        .transpose()?
        .unwrap_or(128);
    if !(1..=512).contains(&limit) {
        return Err("limit must be between 1 and 512".into());
    }
    let after = args
        .get("after_object_id")
        .map(|v| {
            v.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or("after_object_id must be a signed integer string")
        })
        .transpose()?;
    let mut rows: Vec<_> = traffic
        .api_light_controllers()
        .into_iter()
        .filter(|(id, _)| after.is_none_or(|a| *id > a))
        .take(limit as usize + 1)
        .collect();
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = more.then(|| rows.last().unwrap().0.to_string());
    Ok(json!({"items":rows.into_iter().map(|(id,c)| {
        let mut row = light_row(id,c);
        row["controller_index"] = json!(traffic.api_light_controller_index(id));
        row["generation"] = json!(traffic.api_generation.to_string());
        row
    }).collect::<Vec<_>>(),"generation":traffic.api_generation.to_string(),"next_after_object_id":next}))
}

fn light_access(app: &mut App, op: &str, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let object = text(args, "object_id")?
        .parse::<i64>()
        .map_err(|_| "invalid object_id")?;
    if text(args, "generation")? != traffic.api_generation.to_string() {
        return Err("stale light generation; list controllers again".into());
    }
    let generation = traffic.api_generation.to_string();
    let controller_index = traffic.api_light_controller_index(object);
    let ctl = traffic
        .api_light_controller_mut(object)
        .ok_or("light controller is not loaded")?;
    if op == "traffic.lights.set" {
        light_state_apply(ctl, args)?;
    }
    let requests = args.get("requests").map(|_| ctl.request.clone());
    let held = args.get("held").and_then(Value::as_bool);
    if op == "traffic.lights.set" {
        traffic.api_queue_light_state(object, requests, held);
        return Ok(json!({"object_id":object.to_string(),"generation":generation,"updated":true}));
    }
    let mut row = light_row(object, ctl);
    row["controller_index"] = json!(controller_index);
    row["generation"] = json!(generation);
    Ok(row)
}

fn light_state_apply(
    ctl: &mut omsi_sim::traffic::TrafficLightController,
    args: &Value,
) -> Result<(), String> {
    let fields = [
        "time", "held", "running", "requests", "cycle", "offset", "lights", "approach", "stops",
    ];
    keys(
        args,
        &[
            "session_id",
            "generation",
            "object_id",
            "expected_phase_indices",
            "time",
            "held",
            "running",
            "requests",
            "cycle",
            "offset",
            "lights",
            "approach",
            "stops",
        ],
    )?;
    if !fields.iter().any(|field| args.get(field).is_some()) {
        return Err("no writable light state supplied".into());
    }
    // Compare the program observed by the plugin with the live phases on the
    // simulation thread, before applying any clock, demand or program changes.
    if let Some(value) = args.get("expected_phase_indices") {
        let values = value
            .as_array()
            .ok_or("expected_phase_indices must be an array")?;
        if values.len() != ctl.lights.len() {
            return Err("expected_phase_indices must match the controller's light count".into());
        }
        let expected = values
            .iter()
            .zip(&ctl.lights)
            .map(|(value, phases)| {
                value
                    .as_i64()
                    .and_then(|index| i32::try_from(index).ok())
                    .filter(|index| {
                        if phases.is_empty() {
                            *index == -1
                        } else {
                            *index >= 0 && (*index as usize) < phases.len()
                        }
                    })
                    .ok_or("expected_phase_indices must contain valid integer phase indices")
            })
            .collect::<Result<Vec<_>, _>>()?;
        if expected
            .iter()
            .enumerate()
            .any(|(index, phase)| *phase != ctl.phase_index(index))
        {
            return Err(
                "traffic light phase precondition failed: read the current controller and replan"
                    .into(),
            );
        }
    }
    // Validate a clone, then replace the controller once. A bad phase/stop cannot
    // leave the cycle or requests from the same command partially applied.
    let mut edited = ctl.clone();
    if let Some(value) = args.get("cycle") {
        edited.cycle = number(value, "cycle", 0.0, 86400.0)?;
    }
    if let Some(value) = args.get("offset") {
        edited.offset = number(value, "offset", -86400.0, 86400.0)?;
    }
    if let Some(value) = args.get("lights") {
        let lights = value.as_array().ok_or("lights must be an array")?;
        if lights.len() != ctl.lights.len() {
            return Err("cannot change the controller's light count".into());
        }
        let mut result = Vec::new();
        for phases in lights {
            let phases = phases
                .as_array()
                .ok_or("each light must be an array of phases")?;
            if phases.len() > 128 {
                return Err("a light can contain at most 128 phases".into());
            }
            let mut result_phases = Vec::new();
            for phase in phases {
                keys(phase, &["state", "duration"])?;
                let state = phase
                    .get("state")
                    .and_then(Value::as_i64)
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or("phase state must be an i32")?;
                let duration = number(
                    phase.get("duration").ok_or("missing phase duration")?,
                    "duration",
                    0.0,
                    86400.0,
                )?;
                result_phases.push((state, duration));
            }
            if result_phases
                .iter()
                .map(|(_, duration)| *duration as f64)
                .sum::<f64>()
                > 86400.0
            {
                return Err("phase durations exceed one day".into());
            }
            result.push(result_phases);
        }
        edited.lights = result;
    }
    if let Some(value) = args.get("approach") {
        let values = value.as_array().ok_or("approach must be an array")?;
        if values.len() != ctl.lights.len() {
            return Err("approach must match the light count".into());
        }
        edited.approach = values
            .iter()
            .map(|v| {
                if v.is_null() {
                    Ok(None)
                } else {
                    number(v, "approach", 0.0, 10000.0).map(Some)
                }
            })
            .collect::<Result<_, String>>()?;
    }
    if let Some(value) = args.get("stops") {
        let stops = value.as_array().ok_or("stops must be an array")?;
        if stops.len() > 512 {
            return Err("at most 512 stop/jump points are supported".into());
        }
        let mut result = Vec::new();
        for stop in stops {
            keys(stop, &["light", "time", "if_request", "jump_to"])?;
            let light = stop
                .get("light")
                .and_then(Value::as_u64)
                .filter(|i| *i < ctl.lights.len() as u64)
                .ok_or("stop light is outside the controller")? as usize;
            let time = number(
                stop.get("time").ok_or("missing stop time")?,
                "time",
                0.0,
                edited.cycle_len() as f32,
            )?;
            let if_request = stop
                .get("if_request")
                .and_then(Value::as_bool)
                .ok_or("if_request must be a boolean")?;
            let jump_to = stop
                .get("jump_to")
                .filter(|v| !v.is_null())
                .map(|v| number(v, "jump_to", 0.0, edited.cycle_len() as f32))
                .transpose()?;
            result.push(omsi_sim::traffic::LightStop {
                light,
                time,
                if_request,
                jump_to,
            });
        }
        edited.stops = result;
    }
    if let Some(value) = args.get("requests") {
        let values = value.as_array().ok_or("requests must be an array")?;
        if values.len() != ctl.lights.len() {
            return Err("requests must match the controller's light count".into());
        }
        edited.request = values
            .iter()
            .map(|v| v.as_bool().ok_or("requests must contain booleans"))
            .collect::<Result<_, _>>()?;
    }
    let time = args
        .get("time")
        .map(|v| {
            v.as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or("time must be finite and nonnegative")
        })
        .transpose()?
        .unwrap_or(edited.time);
    if fields[..].iter().any(|field| {
        matches!(*field, "time" | "cycle" | "offset" | "lights" | "stops")
            && args.get(field).is_some()
    }) {
        edited.seek(time);
    }
    if let Some(value) = args.get("held") {
        edited.held = value.as_bool().ok_or("held must be a boolean")?;
    }
    if let Some(value) = args.get("running") {
        edited.set_running(value.as_bool().ok_or("running must be a boolean")?);
    }
    *ctl = edited;
    Ok(())
}

fn number(value: &Value, name: &str, min: f32, max: f32) -> Result<f32, String> {
    let number = value
        .as_f64()
        .ok_or_else(|| format!("{name} must be a number"))? as f32;
    if !number.is_finite() || !(min..=max).contains(&number) {
        return Err(format!("{name} must be finite and between {min} and {max}"));
    }
    Ok(number)
}

fn path_row(index: usize, lane: &omsi_sim::traffic::Lane) -> Value {
    json!({"id":format!("path:{index}"),"index":index,"source":lane.key.map(|k|
        json!({"tile":[k.tile.0,k.tile.1],"map_id":k.id.to_string(),"path_index":k.path})),
        "file_name":lane.name,"kind":format!("{:?}",lane.kind).to_lowercase(),"reversed":lane.reversed,
        "width_m":lane.width,"length_m":lane.length(),"speed_limit_kmh":lane.speed_limit_kmh,
        "priority":lane.priority,"density":lane.density,"group_density":lane.group_density,
        "no_cars":lane.no_cars,"no_trucks":!lane.rule_trucks,"turn":lane.turn,
        "rule_bus":lane.rule_bus,"rule_trucks":lane.rule_trucks,
        "next":lane.next,"left":lane.left,"right":lane.right,"block_paths":lane.blocks,
        "traffic_light":lane.traffic_light.map(|(controller,index)|json!({"controller_index":controller,"light_index":index})),
    })
}

fn path_list(app: &App, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_ref().ok_or("traffic is not loaded")?;
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().ok_or("limit must be an integer"))
        .transpose()?
        .unwrap_or(128);
    if !(1..=512).contains(&limit) {
        return Err("limit must be between 1 and 512".into());
    }
    let after = args
        .get("after_index")
        .map(|v| v.as_u64().ok_or("after_index must be an integer"))
        .transpose()?;
    let tile = args
        .get("tile")
        .map(|v| {
            let coordinates = v
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or("tile must contain x,y")?;
            let x = coordinates[0]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("invalid tile x")?;
            let y = coordinates[1]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("invalid tile y")?;
            Ok::<_, String>((x, y))
        })
        .transpose()?;
    let mut rows: Vec<_> = traffic
        .net
        .lanes
        .iter()
        .enumerate()
        .filter(|(index, lane)| {
            after.is_none_or(|a| *index as u64 > a)
                && tile.is_none_or(|tile| lane.key.is_some_and(|k| k.tile == tile))
        })
        .take(limit as usize + 1)
        .collect();
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = more.then(|| rows.last().unwrap().0);
    Ok(
        json!({"generation":path_generation(traffic),"items":rows.into_iter().map(|(i,l)|path_row(i,l)).collect::<Vec<_>>(),"next_after_index":next}),
    )
}

fn path_generation(traffic: &crate::traffic::Traffic) -> String {
    format!(
        "network:{}:{}",
        traffic.api_generation, traffic.lanes_generation
    )
}

fn path_access(app: &mut App, op: &str, args: &Value) -> Result<Value, String> {
    let traffic = app.traffic.as_mut().ok_or("traffic is not loaded")?;
    let index = text(args, "id")?
        .strip_prefix("path:")
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or("invalid path handle")?;
    if text(args, "generation")? != path_generation(traffic) {
        return Err("stale path generation; list paths again".into());
    }
    let lane = traffic
        .net
        .lanes
        .get_mut(index)
        .ok_or("path is not loaded")?;
    if op == "traffic.paths.set" {
        path_rules_apply(lane, args)?;
        traffic.api_refresh_path_rules();
        return Ok(
            json!({"id":format!("path:{index}"),"generation":path_generation(traffic),"updated":true}),
        );
    }
    let lane = &traffic.net.lanes[index];
    let mut result = path_row(index, lane);
    result["generation"] = json!(path_generation(traffic));
    result["points"] = json!(
        lane.points
            .iter()
            .map(|p| [p.x, p.y, p.z])
            .collect::<Vec<_>>()
    );
    result["headings_deg"] = json!(lane.headings);
    result["curvature_per_m"] = json!(lane.curvature);
    result["distances_m"] = json!(lane.dist);
    result["conflicts"] = json!(traffic.net.conflicts.get(index));
    result["previous"] = json!(traffic.net.prev.get(index));
    result["reserved_by"] = json!(
        traffic
            .cars
            .iter()
            .filter(|c| c.reserved.contains(&index))
            .map(|c| format!("traffic:{}", c.api_id))
            .collect::<Vec<_>>()
    );
    Ok(result)
}

fn path_rules_apply(lane: &mut omsi_sim::traffic::Lane, args: &Value) -> Result<(), String> {
    keys(args, &["session_id", "id", "generation", "values"])?;
    let values = args
        .get("values")
        .and_then(Value::as_object)
        .filter(|v| !v.is_empty())
        .ok_or("values must be a nonempty object")?;
    if values.contains_key("no_trucks") && values.contains_key("rule_trucks") {
        return Err("use either no_trucks or rule_trucks, not both".into());
    }
    // Clone so all field types and bounds are checked before native rules change.
    let mut edited = lane.clone();
    for (name, value) in values {
        match name.as_str() {
            "speed_limit_kmh" => edited.speed_limit_kmh = number(value, name, 0.0, 2000.0)?,
            "priority" => edited.priority = number(value, name, 0.0, 65535.0)?,
            "density" => edited.density = number(value, name, 0.0, 1000.0)?,
            "no_cars" => edited.no_cars = value.as_bool().ok_or("no_cars must be a boolean")?,
            "no_trucks" => {
                edited.rule_trucks = !value.as_bool().ok_or("no_trucks must be a boolean")?
            }
            "rule_bus" => edited.rule_bus = value.as_bool().ok_or("rule_bus must be a boolean")?,
            "rule_trucks" => {
                edited.rule_trucks = value.as_bool().ok_or("rule_trucks must be a boolean")?
            }
            "turn" => {
                edited.turn = value
                    .as_i64()
                    .filter(|v| (0..=2).contains(v))
                    .ok_or("turn must be 0, 1 or 2")? as i32
            }
            _ => return Err(format!("unsupported path property {name}")),
        }
    }
    *lane = edited;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_case_names_and_out_of_range_numbers_are_rejected() {
        assert!(ScriptUpdate::parse(&json!({"values":{"rychlost":20,"RYCHLOST":30}})).is_err());
        assert!(ScriptUpdate::parse(&json!({"values":{"rychlost":1e300}})).is_err());
        assert!(ScriptUpdate::parse(&json!({"strings":{"label":42}})).is_err());
        assert!(ScriptUpdate::parse(&json!({"values":{}})).is_err());
    }

    #[test]
    fn whole_scenery_update_is_validated_before_any_write() {
        let program = std::sync::Arc::new(omsi_script::compile(&omsi_script::CompileInput {
            builtin_vars: vec!["rychlost".into()],
            ..Default::default()
        }));
        let mut inst = SceneryInstance::new(program, &[], Default::default(), &[]);
        inst.set_var("rychlost", 7.0);
        let args = json!({"values":{"rychlost":55},"strings":{"missing":"hello"}});
        assert!(scenery_script_access(&mut inst, "scenery.set", &args).is_err());
        assert_eq!(inst.var("rychlost"), Some(7.0));
        scenery_script_access(&mut inst, "scenery.set", &json!({"values":{"RYCHLOST":55}}))
            .unwrap();
        assert_eq!(inst.var("rychlost"), Some(55.0));
    }

    #[test]
    fn handles_are_typed_and_pagination_is_stable_when_rows_disappear() {
        assert!(handle("traffic:12", "scenery:").is_err());
        assert!(handle("scenery:0", "scenery:").is_err());
        let first = paged(
            vec![(8, json!(8)), (2, json!(2)), (5, json!(5))],
            2,
            "scenery:",
        );
        assert_eq!(first["items"], json!([2, 5]));
        assert_eq!(first["next_after_id"], "scenery:5");
        assert_eq!(
            page(&json!({"after_id":"scenery:5","limit":2}), "scenery:").unwrap(),
            (5, 2)
        );
    }

    #[test]
    fn traffic_light_updates_reject_partial_or_unsupported_writes() {
        let mut ctl = omsi_sim::traffic::TrafficLightController::new(vec![vec![(3, 30.0)]], 30.0);
        ctl.time = 7.0;
        assert!(light_state_apply(&mut ctl, &json!({"time":15,"requests":[true,false]})).is_err());
        assert_eq!(ctl.time, 7.0);
        assert!(light_state_apply(&mut ctl, &json!({"time":15,"invented_property":true})).is_err());
        assert_eq!(ctl.time, 7.0);
        light_state_apply(&mut ctl, &json!({"time":15,"held":true,"requests":[true]})).unwrap();
        assert_eq!(ctl.time, 15.0);
        assert!(ctl.held);
        assert_eq!(ctl.request, vec![true]);
    }

    #[test]
    fn light_program_edits_are_atomic_and_drive_real_phase_lookup() {
        let mut ctl = omsi_sim::traffic::TrafficLightController::new(vec![vec![(0, 30.0)]], 30.0);
        assert!(
            light_state_apply(
                &mut ctl,
                &json!({"cycle":60,"lights":[[{"state":3,"duration":-1}]]})
            )
            .is_err()
        );
        assert_eq!(ctl.cycle, 30.0);
        light_state_apply(
            &mut ctl,
            &json!({"cycle":60,"time":35,
            "lights":[[{"state":0,"duration":30},{"state":3,"duration":30}]],
            "stops":[{"light":0,"time":40,"if_request":true,"jump_to":null}]}),
        )
        .unwrap();
        assert_eq!(ctl.state(0), 3);
        ctl.advance(10.0);
        assert_eq!(ctl.time, 40.0);
        assert!(ctl.held);
        light_state_apply(&mut ctl, &json!({"stops":[]})).unwrap();
        assert!(ctl.stops.is_empty());
        ctl.advance(1.0);
        assert_eq!(ctl.time, 41.0);
    }

    #[test]
    fn signal_group_clock_controls_persist_between_plugin_updates() {
        let mut ctl = omsi_sim::traffic::TrafficLightController::new(
            vec![vec![(0, 10.0), (3, 2.0), (6, 15.0), (9, 3.0)]],
            30.0,
        );
        ctl.names = vec!["bus_approach".into()];
        light_state_apply(&mut ctl, &json!({"running":false})).unwrap();
        ctl.start(25.0);
        ctl.advance(1.0);
        assert_eq!(
            ctl.time, 0.0,
            "a first-step pause preserves the reported time"
        );
        light_state_apply(&mut ctl, &json!({"running":true})).unwrap();
        ctl.start(26.0);
        ctl.advance(1.0);
        assert_eq!(ctl.time, 1.0, "resume continues from the paused clock");
        light_state_apply(&mut ctl, &json!({"time":14,"running":false})).unwrap();
        for _ in 0..180 {
            ctl.advance(1.0 / 60.0);
        }
        assert_eq!(ctl.time, 14.0);
        assert_eq!(ctl.state(0), 6);
        assert!(ctl.held);
        let state = light_row(123, &ctl);
        assert_eq!(state["names"], json!(["bus_approach"]));
        assert_eq!(state["running"], false);
        assert!(light_state_apply(&mut ctl, &json!({"time":0,"running":1})).is_err());
        assert_eq!(ctl.time, 14.0);
        assert!(!ctl.running);
        light_state_apply(&mut ctl, &json!({"time":26,"running":true})).unwrap();
        ctl.advance(1.5);
        assert_eq!(ctl.state(0), 9);
        assert_eq!(ctl.time, 27.5);
        assert!(!ctl.held);
    }

    #[test]
    fn traffic_light_phase_preconditions_reject_stale_updates_atomically() {
        let mut ctl = omsi_sim::traffic::TrafficLightController::new(
            vec![
                vec![(0, 10.0), (3, 2.0), (6, 10.0), (9, 3.0), (0, 5.0)],
                vec![(0, 22.0), (6, 8.0)],
            ],
            30.0,
        );
        ctl.seek(21.99);
        let observed = light_row(123, &ctl)["phase_indices"].clone();
        assert_eq!(observed, json!([2, 0]));
        ctl.advance(0.02);
        assert_eq!(ctl.state(0), 9);
        let before = light_row(123, &ctl);
        let error = light_state_apply(
            &mut ctl,
            &json!({
                "expected_phase_indices":observed,"time":12,"running":false,
                "held":true,"requests":[true,true],"cycle":60,"offset":1,
                "lights":[[{"state":6,"duration":60}],[{"state":0,"duration":60}]],
                "approach":[1,2],"stops":[{"light":0,"time":20,"if_request":true}],
            }),
        )
        .unwrap_err();
        assert!(error.starts_with("traffic light phase precondition failed"));
        assert_eq!(
            light_row(123, &ctl),
            before,
            "a stale plan must not rewind yellow or alter any state"
        );
        // The complete group is guarded, including an approach other than the
        // one the plugin wants to extend.
        assert!(
            light_state_apply(
                &mut ctl,
                &json!({
                    "expected_phase_indices":[3,0],"time":12,
                })
            )
            .unwrap_err()
            .starts_with("traffic light phase precondition failed")
        );
        assert_eq!(light_row(123, &ctl), before);
        light_state_apply(
            &mut ctl,
            &json!({
                "expected_phase_indices":before["phase_indices"],"time":12,"running":false,
            }),
        )
        .unwrap();
        assert_eq!(ctl.time, 12.0);
        assert!(!ctl.running);
    }

    #[test]
    fn traffic_light_phase_preconditions_validate_shape_and_indices_before_writes() {
        let mut ctl =
            omsi_sim::traffic::TrafficLightController::new(vec![vec![(6, 10.0), (6, 20.0)]], 30.0);
        ctl.seek(15.0);
        let before = light_row(123, &ctl);
        for invalid in [
            json!(null),
            json!(0),
            json!([]),
            json!([1, 1]),
            json!([true]),
            json!(["1"]),
            json!([1.5]),
            json!([-1]),
            json!([2]),
            json!([2147483648_i64]),
        ] {
            let error = light_state_apply(
                &mut ctl,
                &json!({
                    "expected_phase_indices":invalid,"time":0,"running":false,
                }),
            )
            .unwrap_err();
            assert!(error.starts_with("expected_phase_indices"), "{error}");
            assert_eq!(light_row(123, &ctl), before);
        }
        // Equal color codes in adjacent phases do not make an old phase index current.
        assert!(
            light_state_apply(
                &mut ctl,
                &json!({
                    "expected_phase_indices":[0],"time":0,
                })
            )
            .unwrap_err()
            .starts_with("traffic light phase precondition failed")
        );
        assert_eq!(light_row(123, &ctl), before);
        // An authored empty program is reported as phase -1 and can be guarded too.
        let mut empty = omsi_sim::traffic::TrafficLightController::new(vec![vec![]], 30.0);
        light_state_apply(
            &mut empty,
            &json!({"expected_phase_indices":[-1],"running":false}),
        )
        .unwrap();
        assert!(!empty.running);
    }

    #[test]
    fn path_rules_validate_every_field_before_changing_native_lane() {
        let mut lane = omsi_sim::traffic::LaneBuilder::arc(
            glam::DVec3::ZERO,
            0.0,
            50.0,
            0.0,
            0.0,
            omsi_sim::traffic::LaneKind::Street,
            3.0,
        );
        assert!(
            path_rules_apply(
                &mut lane,
                &json!({"values":{"speed_limit_kmh":30,"priority":"high"}})
            )
            .is_err()
        );
        assert_eq!(lane.speed_limit_kmh, 50.0);
        assert!(path_rules_apply(&mut lane, &json!({"values":{"density":0,"missing":1}})).is_err());
        assert_eq!(lane.density, 1.0);
        path_rules_apply(
            &mut lane,
            &json!({"values":{"speed_limit_kmh":30,"priority":192,"no_cars":true}}),
        )
        .unwrap();
        assert_eq!(lane.speed_limit_kmh, 30.0);
        assert_eq!(lane.priority, 192.0);
        assert!(lane.no_cars);
        path_rules_apply(
            &mut lane,
            &json!({"values":{"no_trucks":false,"rule_bus":true}}),
        )
        .unwrap();
        assert!(lane.allows(2));
        assert!(lane.allows(3));
        assert_eq!(path_row(0, &lane)["no_trucks"], false);
        assert!(
            path_rules_apply(
                &mut lane,
                &json!({"values":{"no_trucks":true,"rule_trucks":true,"rule_bus":false}})
            )
            .is_err()
        );
        assert!(lane.rule_bus && lane.rule_trucks);
        path_rules_apply(&mut lane, &json!({"values":{"rule_trucks":false}})).unwrap();
        assert!(!lane.allows(3));
        assert_eq!(path_row(0, &lane)["no_trucks"], true);
    }

    #[test]
    fn ai_driver_changes_are_atomic_and_reject_unmapped_state() {
        let mut state = omsi_sim::traffic::AiState::new(0, 0.0, 1);
        let initial_accel = state.accel;
        assert!(
            behavior_apply(
                &mut state,
                &json!({"values":{"accel_mps2":3,"headway_s":0}})
            )
            .is_err()
        );
        assert_eq!(state.accel, initial_accel);
        assert!(
            behavior_apply(&mut state, &json!({"values":{"accel_mps2":3,"position":0}})).is_err()
        );
        assert_eq!(state.accel, initial_accel);
        behavior_apply(
            &mut state,
            &json!({"values":{"accel_mps2":3,"headway_s":2,"max_speed_kmh":40}}),
        )
        .unwrap();
        assert_eq!(state.accel, 3.0);
        assert_eq!(state.headway, 2.0);
        assert_eq!(state.max_speed_kmh, 40.0);
    }

    #[test]
    fn spawning_only_accepts_vehicle_content_files() {
        assert!(vehicle_asset_path("Vehicles\\SD202\\D92.bus").is_ok());
        assert!(vehicle_asset_path("Vehicles/Cars/car.OVH").is_ok());
        for path in [
            "../Vehicles/car.ovh",
            "Vehicles/../car.ovh",
            "C:\\Vehicles\\car.ovh",
            "Vehicles/car.exe",
            "Scripts/file.bus",
            "Vehicles/car.zug",
            "",
        ] {
            assert!(vehicle_asset_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn service_commands_obey_native_phase_and_update_both_departure_fields() {
        let mut service = crate::bus_service::BusService::new(vec![crate::bus_service::Stop {
            ri: 0,
            s: 10.0,
            bay: 0.0,
            depart: 120.0,
            id: 42,
            side: 0.0,
        }]);
        assert!(
            service_apply(&mut service, "traffic.service.hold", &json!({"seconds":10})).is_err()
        );
        assert_eq!(service.boarding, 0.0);
        service.phase = crate::bus_service::Phase::Boarding;
        service_apply(&mut service, "traffic.service.hold", &json!({"seconds":10})).unwrap();
        assert_eq!(service.boarding, 10.0);
        assert!(
            service_apply(
                &mut service,
                "traffic.service.set_departure",
                &json!({"departure_seconds":-1})
            )
            .is_err()
        );
        assert_eq!(service.stops.front().unwrap().depart, 120.0);
        service_apply(
            &mut service,
            "traffic.service.set_departure",
            &json!({"departure_seconds":180}),
        )
        .unwrap();
        assert_eq!(service.stops.front().unwrap().depart, 180.0);
        assert_eq!(service.leave_at, 180.0);
    }
}
