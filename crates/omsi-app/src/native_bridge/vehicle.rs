//! The bridge's deliberately small, current-vehicle script contract.
use super::protocol::{validate_values, Request};
use crate::{scene::World, App};
use omsi_sim::{VehicleInstance, VehicleType};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

struct VehicleIdentity {
    id: u64,
    generation: u64,
    ty: Weak<VehicleType>,
}

pub(super) struct Context {
    session: String,
    // Weak references distinguish reloads without keeping their resources alive.
    map: Option<Weak<World>>,
    vehicle: Option<VehicleIdentity>,
    next_generation: u64,
}

pub(super) fn random_id() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn same_instance<T>(current: Option<&Arc<T>>, previous: Option<&Weak<T>>) -> bool {
    current.map(Arc::as_ptr) == previous.map(Weak::as_ptr)
}

impl Context {
    pub(super) fn new() -> Self {
        Self {
            session: random_id(),
            map: None,
            vehicle: None,
            next_generation: 0,
        }
    }

    pub(super) fn session(&self) -> &str {
        &self.session
    }

    /// Called on the game thread before accepting any queued writes.
    pub(super) fn refresh(&mut self, app: &App) -> bool {
        let changed = !same_instance(app.world.as_ref(), self.map.as_ref());
        if changed {
            self.map = app.world.as_ref().map(Arc::downgrade);
            self.session = random_id();
            self.vehicle = None;
        }
        self.refresh_vehicle(app.player.as_ref().map(|p| (p.uid, &p.vehicle.ty)));
        changed
    }

    fn refresh_vehicle(&mut self, current: Option<(u64, &Arc<VehicleType>)>) {
        let Some((id, ty)) = current else {
            self.vehicle = None;
            return;
        };
        if self
            .vehicle
            .as_ref()
            .is_none_or(|old| old.id != id || !same_instance(Some(ty), Some(&old.ty)))
        {
            self.next_generation = self.next_generation.wrapping_add(1).max(1);
            self.vehicle = Some(VehicleIdentity {
                id,
                generation: self.next_generation,
                ty: Arc::downgrade(ty),
            });
        }
    }

    fn check_identity(&self, session: &str, id: u64, generation: u64) -> Result<(), String> {
        if session != self.session {
            return Err("stale session; read a new snapshot".into());
        }
        let current = self
            .vehicle
            .as_ref()
            .ok_or("player vehicle is not loaded")?;
        if id != current.id || generation != current.generation {
            return Err("stale vehicle; read a new snapshot".into());
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, app: &App) -> Value {
        let vehicle = app
            .player
            .as_ref()
            .zip(self.vehicle.as_ref())
            .map(|(p, identity)| vehicle_snapshot(&p.vehicle, p.uid, identity.generation));
        let (day, month) = app.clock.day_month();
        let seconds = app.clock.time.max(0.0) as u32;
        json!({"session_id":self.session, "capabilities":["variables", "invalidate_texture"],
            "native_operations":["vehicle.set_variables", "vehicle.trigger"],
            "game_root":app.args.root.to_string_lossy(),
            "map_name":app.world.as_ref().map(|w| w.global.name.as_str()).unwrap_or(""),
            "clock":{"hour":seconds/3600,"minute":seconds/60%60,"second":app.clock.time.rem_euclid(60.0),
                "day":day,"month":month,"year":app.clock.year,"day_of_year":app.clock.day_of_year,
                "service_seconds":app.clock.time}, "paused":app.paused, "vehicle":vehicle})
    }

    pub(super) fn apply(&self, app: &mut App, request: &Request) -> Result<Value, String> {
        if request.op == "snapshot" {
            return Ok(self.snapshot(app));
        }
        // A request never follows the driver into a different vehicle or reloaded map.
        if !same_instance(app.world.as_ref(), self.map.as_ref()) {
            return Err("stale session; read a new snapshot".into());
        }
        let normalized;
        let (id, generation, update) = match request.op.as_str() {
            "set_variables" | "invalidate_texture" => {
                if request.args.is_some()
                    || (request.op == "set_variables" && !request.path.is_empty())
                    || (request.op == "invalidate_texture"
                        && (!request.values.is_empty() || !request.strings.is_empty()))
                {
                    return Err("unexpected operation arguments".into());
                }
                (request.vehicle_id, request.generation, request)
            }
            "vehicle.set_variables" | "vehicle.trigger" => {
                if request.vehicle_id != 0
                    || request.generation != 0
                    || !request.values.is_empty()
                    || !request.strings.is_empty()
                    || !request.path.is_empty()
                {
                    return Err("named operations take their vehicle arguments in args".into());
                }
                let args = named_args(request)?;
                let id = integer_arg(args, "id")?;
                let generation = integer_arg(args, "generation")?;
                normalized = named_update(request, args)?;
                (id, generation, &normalized)
            }
            _ => return Err("unsupported vehicle operation".into()),
        };
        self.check_identity(&request.session_id, id, generation)?;
        let player = app.player.as_mut().ok_or("player vehicle is not loaded")?;
        if player.uid != id
            || !same_instance(
                Some(&player.vehicle.ty),
                self.vehicle.as_ref().map(|v| &v.ty),
            )
        {
            return Err("stale vehicle; read a new snapshot".into());
        }
        match request.op.as_str() {
            "set_variables" | "vehicle.set_variables" => {
                apply_variables(&mut player.vehicle, update)?
            }
            "vehicle.trigger" => {
                let name = named_args(request)?
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("trigger name is required")?;
                fire_trigger(&mut player.vehicle, name)?;
            }
            "invalidate_texture" => {
                let world = app.world.as_ref().ok_or("map is not loaded")?;
                let renderer = app.renderer.as_ref().ok_or("renderer is not available")?;
                let scene = app.scene.as_mut().ok_or("scene is not available")?;
                world.refresh_vehicle_texture(renderer, scene, player, &request.path)?;
            }
            _ => unreachable!(),
        }
        Ok(Value::Null)
    }
}

fn named_args(request: &Request) -> Result<&serde_json::Map<String, Value>, String> {
    let args = request
        .args
        .as_ref()
        .and_then(Value::as_object)
        .ok_or("args must be an object")?;
    let fields: &[&str] = if request.op == "vehicle.trigger" {
        &["id", "generation", "name"]
    } else {
        &["id", "generation", "values", "strings"]
    };
    if let Some(key) = args.keys().find(|k| !fields.contains(&k.as_str())) {
        return Err(format!("unexpected argument: {key}"));
    }
    Ok(args)
}

fn integer_arg(args: &serde_json::Map<String, Value>, name: &str) -> Result<u64, String> {
    args.get(name)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|v| *v != 0)
        .ok_or_else(|| format!("{name} from the current snapshot is required"))
}

fn named_update(
    request: &Request,
    args: &serde_json::Map<String, Value>,
) -> Result<Request, String> {
    serde_json::from_value(
        json!({"protocol":request.protocol,"token":"","request_id":request.request_id,
        "op":request.op,"values":args.get("values").cloned().unwrap_or(json!({})),
        "strings":args.get("strings").cloned().unwrap_or(json!({}))}),
    )
    .map_err(|e| format!("invalid variable update: {e}"))
}

/// Validate the complete numeric/string batch before touching either storage array.
fn apply_variables(vehicle: &mut VehicleInstance, request: &Request) -> Result<(), String> {
    validate_values(request)?;
    if request.values.is_empty() && request.strings.is_empty() {
        return Err("variable update is empty".into());
    }
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
    Ok(())
}

fn fire_trigger(vehicle: &mut VehicleInstance, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 256 || vehicle.ty.program.trigger(name).is_none() {
        return Err(format!("trigger is unavailable: {name}"));
    }
    // Use the VM's normal trigger entry: it seeds its numeric stack with 1. The
    // caller explicitly fires an _off trigger if the bus's script needs one.
    if !vehicle.trigger(name) {
        return Err(format!("trigger is unavailable or failed: {name}"));
    }
    Ok(())
}

fn vehicle_snapshot(vehicle: &VehicleInstance, id: u64, generation: u64) -> Value {
    let mut variables: BTreeMap<_, _> = vehicle
        .ty
        .program
        .var_names
        .iter()
        .zip(&vehicle.state.vars)
        .filter(|(_, value)| value.is_finite())
        .map(|(name, value)| (name.clone(), *value))
        .collect();
    for (index, value) in vehicle
        .state
        .vars
        .iter()
        .enumerate()
        .skip(vehicle.ty.program.var_names.len())
    {
        if value.is_finite() {
            if let Some(name) = vehicle.var_name(index) {
                variables.insert(name.to_string(), *value);
            }
        }
    }
    let strings: BTreeMap<_, _> = vehicle
        .ty
        .program
        .str_var_names
        .iter()
        .zip(&vehicle.state.str_vars)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    json!({"id":id,"generation":generation,"file_name":vehicle.ty.def.path.to_string_lossy(),
        "friendly_name":format!("{} {}",vehicle.ty.def.manufacturer,vehicle.ty.def.type_name).trim(),
        "variables":variables,"strings":strings,"triggers":vehicle.ty.program.trigger_names()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("openomsi-bridge-vehicle-{}", random_id()));
            std::fs::create_dir_all(&root).unwrap();
            for (name, text) in [
                ("bus.bus", "[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n[stringvarnamelist]\n1\nstrings.txt\n[script]\n1\nscript.osc\n[mass]\n10000\n"),
                ("model.cfg", ""), ("vars.txt", "card_ready\ntrigger_count\n"), ("strings.txt", "driver_name\n"),
                ("script.osc", "{trigger:card_insert}\n{if}\n(L.L.trigger_count) 1 + (S.L.trigger_count)\n{endif}\n{end}\n{trigger:card_insert_off}\n0 (S.L.card_ready)\n{end}\n"),
            ] { std::fs::write(root.join(name), text).unwrap(); }
            Self(root)
        }
        fn ty(&self) -> Arc<VehicleType> {
            Arc::new(VehicleType::load(&self.0, &self.0.join("bus.bus")).unwrap())
        }
        fn vehicle(&self) -> VehicleInstance {
            VehicleInstance::new(
                self.ty(),
                omsi_sim::VehicleHost::new(omsi_sim::SimClock::default()),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(path), Ok(root)) =
                (self.0.canonicalize(), std::env::temp_dir().canonicalize())
            {
                if path.starts_with(&root) && path != root {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }
    fn request(values: Value, strings: Value) -> Request {
        serde_json::from_value(json!({"protocol":1,"token":"test","request_id":1,"op":"set_variables","values":values,"strings":strings})).unwrap()
    }

    #[test]
    fn script_batch_is_atomic_and_snapshot_preserves_named_maps() {
        let fixture = Fixture::new();
        let mut vehicle = fixture.vehicle();
        let valid = request(
            json!({"card_ready":1}),
            json!({"driver_name":"Example Driver"}),
        );
        apply_variables(&mut vehicle, &valid).unwrap();
        vehicle.set_engine_var("example_model_value", 0.5);
        let before = vehicle_snapshot(&vehicle, 7, 1);
        assert_eq!(before["variables"]["card_ready"], 1.0);
        assert_eq!(before["variables"]["example_model_value"], 0.5);
        assert_eq!(before["strings"]["driver_name"], "Example Driver");
        assert!(before["triggers"]
            .as_array()
            .unwrap()
            .contains(&json!("card_insert")));
        for invalid in [
            request(json!({"card_ready":0}), json!({"missing":"x"})),
            request(json!({"missing":1}), json!({"driver_name":"Changed"})),
            request(json!({"card_ready":0,"CARD_READY":1}), json!({})),
        ] {
            assert!(apply_variables(&mut vehicle, &invalid).is_err());
            assert_eq!(vehicle_snapshot(&vehicle, 7, 1), before);
        }
        let mut non_finite = request(json!({"card_ready":0}), json!({}));
        non_finite.values.insert("card_ready".into(), f32::INFINITY);
        assert!(apply_variables(&mut vehicle, &non_finite).is_err());
        assert_eq!(vehicle_snapshot(&vehicle, 7, 1), before);
        let mut named = request(json!({}), json!({}));
        named.op = "vehicle.set_variables".into();
        named.args = Some(
            json!({"id":"7","generation":"1","values":{"card_ready":0},"strings":{"driver_name":"Second Driver"}}),
        );
        let args = named_args(&named).unwrap();
        assert_eq!(integer_arg(args, "id").unwrap(), 7);
        apply_variables(&mut vehicle, &named_update(&named, args).unwrap()).unwrap();
        assert_eq!(
            vehicle_snapshot(&vehicle, 7, 1)["strings"]["driver_name"],
            "Second Driver"
        );
        assert_eq!(vehicle.var("card_ready"), Some(0.0));
    }

    #[test]
    fn declared_trigger_runs_once_with_native_stack_and_explicit_off() {
        let fixture = Fixture::new();
        let mut vehicle = fixture.vehicle();
        vehicle.set_var("card_ready", 1.0);
        fire_trigger(&mut vehicle, "card_insert").unwrap();
        assert_eq!(vehicle.var("trigger_count"), Some(1.0));
        assert_eq!(vehicle.var("card_ready"), Some(1.0));
        assert!(fire_trigger(&mut vehicle, "missing").is_err());
        assert_eq!(vehicle.var("trigger_count"), Some(1.0));
        fire_trigger(&mut vehicle, "card_insert_off").unwrap();
        assert_eq!(vehicle.var("card_ready"), Some(0.0));
        let mut named = request(json!({}), json!({}));
        named.op = "vehicle.trigger".into();
        named.args = Some(json!({"id":"7","generation":"1","name":"card_insert","value":0}));
        assert!(
            named_args(&named).is_err(),
            "the API does not invent trigger arguments"
        );
    }

    #[test]
    fn vehicle_switch_removal_and_reload_invalidate_identity() {
        let fixture = Fixture::new();
        let ty = fixture.ty();
        let mut context = Context::new();
        let session = context.session().to_string();
        context.refresh_vehicle(Some((7, &ty)));
        assert!(context.check_identity(&session, 7, 1).is_ok());
        context.refresh_vehicle(Some((7, &ty)));
        assert!(context.check_identity(&session, 7, 1).is_ok());
        context.refresh_vehicle(Some((8, &ty)));
        assert!(context.check_identity(&session, 7, 1).is_err());
        assert!(context.check_identity(&session, 8, 2).is_ok());
        context.refresh_vehicle(None);
        assert!(context.check_identity(&session, 8, 2).is_err());
        context.refresh_vehicle(Some((8, &ty)));
        let replacement = fixture.ty();
        context.refresh_vehicle(Some((8, &replacement)));
        assert!(context.check_identity(&session, 8, 3).is_err());
        assert!(context
            .check_identity("previous-map-session", 8, 4)
            .is_err());
        assert!(context.check_identity(&session, 8, 4).is_ok());
        // Holding a Weak identity prevents allocator address reuse across reloads.
        let map = Arc::new(());
        let previous = Arc::downgrade(&map);
        assert!(same_instance(Some(&map), Some(&previous)));
        drop(map);
        assert!(!same_instance(Some(&Arc::new(())), Some(&previous)));
        assert!(!same_instance(None, Some(&previous)));
    }
}
