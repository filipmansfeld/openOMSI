//! Native driver walking. Commands enter the existing acceleration/cabin/wall/ground
//! movement path; this is not the original plugin's injected collision-box format.
use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) fn new_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkControl {
    forward: f64,
    right: f64,
    speed: f64,
    pub(super) heading: Option<f64>,
    pub(super) jump: bool,
}
impl WalkControl {
    pub(super) fn velocity(self, yaw: f64) -> DVec2 {
        let y = yaw.to_radians();
        let direction = DVec2::new(y.sin(), y.cos()) * self.forward
            + DVec2::new(y.cos(), -y.sin()) * self.right;
        direction / direction.length().max(1.0) * self.speed
    }
}

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        if key != "session_id" && !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported driver walking argument: {key}"));
        }
    }
    Ok(())
}
fn number(args: &Value, key: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && (min..=max).contains(n))
                .ok_or_else(|| format!("{key} must be finite and in {min}..{max}"))
        })
        .transpose()
}
fn control(args: &Value) -> Result<WalkControl, String> {
    keys(
        args,
        &[
            "id",
            "forward",
            "right",
            "speed_metres_per_second",
            "heading_degrees",
            "jump",
        ],
    )?;
    if ![
        "forward",
        "right",
        "speed_metres_per_second",
        "heading_degrees",
        "jump",
    ]
    .iter()
    .any(|key| args.get(key).is_some())
    {
        return Err("driver walking control is empty".into());
    }
    Ok(WalkControl {
        forward: number(args, "forward", -1.0, 1.0)?.unwrap_or(0.0),
        right: number(args, "right", -1.0, 1.0)?.unwrap_or(0.0),
        speed: number(args, "speed_metres_per_second", 0.0, 10.0)?.unwrap_or(WALK),
        heading: number(args, "heading_degrees", -360000.0, 360000.0)?.map(|n| n.rem_euclid(360.0)),
        jump: args
            .get("jump")
            .map(|v| v.as_bool().ok_or("jump must be a boolean"))
            .transpose()?
            .unwrap_or(false),
    })
}
fn bus(bus: BusId) -> Value {
    match bus {
        BusId::Player => json!({"kind":"player"}),
        BusId::Ai(id) => json!({"kind":"ai_or_placed","simulation_id":id.to_string()}),
    }
}
fn snapshot(app: &App) -> Value {
    let Some(f) = app.on_foot.as_ref() else {
        return json!({"active":false,"enabled":app.settings.get_up});
    };
    json!({"active":true,"enabled":app.settings.get_up,"id":format!("walker:{}",f.api_id),
        "position":{"east":f.pos.x,"north":f.pos.y,"up":f.pos.z},"heading_degrees":f.heading,
        "look_yaw_degrees":f.yaw,"look_pitch_degrees":f.pitch,"velocity_world":[f.vel.x,f.vel.y],
        "lift_metres":f.lift,"vertical_velocity_metres_per_second":f.vz,
        "seat":f.seat.map(|(owner,index)|json!({"bus":bus(owner),"index":index})),
        "inside":f.inside.map(|(owner,local)|json!({"bus":bus(owner),"position_model":local.to_array()})),
        "transitioning":f.transit.is_some()||f.arrive.is_some(),"free_camera":f.cam==FootCam::Free,
        "control_pending":f.api_control.is_some()})
}
fn require_id(f: &OnFoot, args: &Value) -> Result<(), String> {
    if args.get("id").and_then(Value::as_str) != Some(format!("walker:{}", f.api_id).as_str()) {
        return Err("stale or missing walker id; call driver.walk.get".into());
    }
    Ok(())
}
pub(crate) fn execute(
    app: &mut App,
    operation: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if !operation.starts_with("driver.walk.") {
        return None;
    }
    Some((|| {
        if operation == "driver.walk.get" {
            keys(args, &[])?;
            return Ok(snapshot(app));
        }
        if !args.get("session_id").is_some_and(Value::is_string) {
            return Err("session_id from the current snapshot is required".into());
        }
        match operation {
            "driver.walk.start" => {
                keys(args, &[])?;
                if app.on_foot.is_some() {
                    return Err("driver is already on foot".into());
                }
                if app.player.is_none() {
                    return Err("player vehicle is not loaded".into());
                }
                if app.world.is_none() {
                    return Err("map is not loaded".into());
                }
                if !app.settings.get_up {
                    return Err("Ability to get up is disabled in the simulator settings".into());
                }
                app.get_up();
                if app.on_foot.is_none() {
                    return Err("native driver walking did not start".into());
                }
                Ok(snapshot(app))
            }
            "driver.walk.stop" => {
                keys(args, &["id"])?;
                require_id(app.on_foot.as_ref().ok_or("driver is not on foot")?, args)?;
                if app.player.is_none() {
                    return Err("there is no player vehicle to return to".into());
                }
                app.back_to_bus();
                Ok(snapshot(app))
            }
            "driver.walk.control" => {
                let command = control(args)?;
                let f = app.on_foot.as_mut().ok_or("driver is not on foot")?;
                require_id(f, args)?;
                if app.paused
                    || f.seat.is_some()
                    || f.transit.is_some()
                    || f.arrive.is_some()
                    || f.cam == FootCam::Free
                {
                    return Err(
                        "walking control requires an unpaused walker standing in first-person view"
                            .into(),
                    );
                }
                if command.jump && (f.inside.is_some() || !f.grounded()) {
                    return Err("jump requires the walker to stand on the outside ground".into());
                }
                f.api_control = Some(command);
                Ok(json!({"id":format!("walker:{}",f.api_id),"queued":true}))
            }
            _ => Err(format!("unsupported driver walking operation: {operation}")),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn walker() -> OnFoot {
        OnFoot {
            api_id: new_id(),
            api_control: None,
            pos: DVec3::ZERO,
            heading: 0.0,
            vel: DVec2::ZERO,
            lift: 0.0,
            vz: 0.0,
            seat: None,
            inside: None,
            cam: FootCam::First,
            yaw: 0.0,
            pitch: 0.0,
            eye: None,
            eye_yaw: 0.0,
            lag: DVec3::ZERO,
            settle: 0.0,
            view_before: "driver".into(),
            kind: 0,
            face_seat: false,
            transit: None,
            arrive: None,
        }
    }
    #[test]
    fn native_driver_control_checks_entire_command_and_lifetime() {
        assert!(control(&json!({"forward":1,"right":2})).is_err());
        assert!(control(&json!({"forward":1,"unknown":true})).is_err());
        assert!(control(&json!({"jump":1})).is_err());
        let command = control(&json!({"forward":1,"right":1,"speed_metres_per_second":2})).unwrap();
        assert!((command.velocity(0.0).length() - 2.0).abs() < 1e-9);
        let a = walker();
        let b = walker();
        assert!(require_id(&b, &json!({"id":format!("walker:{}",a.api_id)})).is_err());
    }
    #[test]
    fn native_driver_control_is_consumed_once_by_actual_movement() {
        let mut app = crate::new_app(
            crate::Args::parse_from(["openomsi", "--root", "."]),
            crate::settings::Settings::default(),
        );
        app.paused = false;
        app.on_foot = Some(walker());
        let id = format!("walker:{}", app.on_foot.as_ref().unwrap().api_id);
        execute(
            &mut app,
            "driver.walk.control",
            &json!({"session_id":"checked-by-dispatcher","id":id,
            "forward":1,"speed_metres_per_second":2,"heading_degrees":90}),
        )
        .unwrap()
        .unwrap();
        app.tick_on_foot(0.1);
        let f = app.on_foot.as_ref().unwrap();
        assert!(f.api_control.is_none());
        assert!(f.pos.x > 0.0);
        assert!(f.pos.y.abs() < 1e-9);
        let previous_speed = f.vel.length();
        app.tick_on_foot(0.1);
        assert!(app.on_foot.as_ref().unwrap().vel.length() < previous_speed);
    }
}
