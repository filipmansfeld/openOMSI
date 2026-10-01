//! Camera operations use the same persistent view/look/zoom state as normal input.
use crate::App;
use omsi_render::Camera;
use serde_json::{json, Value};

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        if key != "session_id" && !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported camera argument: {key}"));
        }
    }
    Ok(())
}
fn number(value: &Value, name: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))
}
fn bounded(value: &Value, name: &str, min: f64, max: f64) -> Result<f32, String> {
    let n = number(value, name)?;
    if !(min..=max).contains(&n) {
        return Err(format!("{name} must be in {min}..{max}"));
    }
    Ok(n as f32)
}

fn snapshot(app: &App) -> Result<Value, String> {
    let c = app.camera.as_ref().ok_or("camera is not loaded")?;
    let aspect = app
        .surface
        .as_ref()
        .map(|s| s.config.width as f32 / s.config.height.max(1) as f32);
    Ok(
        json!({"mode":app.view,"position":{"east":c.position.x,"north":c.position.y,"up":c.position.z},
        "heading_degrees":c.yaw,"pitch_degrees":c.pitch,"roll_degrees":c.roll,
        "fov_degrees":c.fov_deg,"near_metres":c.near,"far_metres":c.far,
        "direction":c.forward().to_array(),"up":c.up().to_array(),"aspect_ratio":aspect,
        "view_projection":aspect.map(|a|c.view_proj(a,c.position).to_cols_array()),
        "matrix_origin":c.position.to_array(),"matrix_order":"column_major","depth":"reversed_z",
        "look_yaw_degrees":app.look.0,"look_pitch_degrees":app.look.1,"orbit_metres":app.orbit,
        "driver_index":app.player.as_ref().map(|p| {
            let n=p.vehicle.ty.def.cameras_driver.len().max(1);
            (p.vehicle.ty.def.camera_std+p.cam_choice.0)%n
        }),"passenger_index":app.player.as_ref().map(|p|p.cam_choice.1)}),
    )
}

fn base_fov(app: &App, camera: &Camera) -> f32 {
    if app.settings.fov >= 20.0 {
        app.settings.fov.min(120.0)
    } else if matches!(app.view.as_str(), "free" | "foot") {
        60.0
    } else if camera.fov_deg.is_finite() && camera.fov_deg > 0.0 {
        camera.fov_deg
    } else {
        60.0
    }
}

fn current_look(app: &App) -> (f32, f32) {
    let key = app.look_key();
    if key == app.look_view {
        app.look
    } else {
        app.view_looks.get(&key).copied().unwrap_or_default()
    }
}

fn select(app: &mut App, args: &Value) -> Result<Value, String> {
    keys(args, &["mode", "index"])?;
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .ok_or("mode is required")?;
    if !["driver", "pax", "outside", "free"].contains(&mode) {
        return Err("mode must be driver, pax, outside or free".into());
    }
    let fallback = app.camera.ok_or("camera is not loaded")?;
    if app.on_foot.is_some() {
        return Err(
            "return the walking driver to the bus before selecting a vehicle/free camera".into(),
        );
    }
    let index = args
        .get("index")
        .map(|v| v.as_u64().ok_or("index must be a nonnegative integer"))
        .transpose()?;
    let mut choice = None;
    if mode != "free" {
        let p = app.player.as_ref().ok_or("player vehicle is not loaded")?;
        if let Some(index) = index {
            let index = usize::try_from(index).map_err(|_| "index is too large")?;
            let n = match mode {
                "driver" => p.vehicle.ty.def.cameras_driver.len(),
                "pax" => p.pax_camera_count(),
                _ => 0,
            };
            if index >= n {
                return Err("camera index is unavailable in this view".into());
            }
            choice = Some(if mode == "driver" {
                (index + n - p.vehicle.ty.def.camera_std % n) % n
            } else {
                index
            });
        }
    } else if index.is_some() {
        return Err("the free camera has no index".into());
    }
    // All validation precedes changes to view state or the saved direction.
    app.sync_view_look();
    if let Some(index) = choice {
        let p = app.player.as_mut().unwrap();
        if mode == "driver" {
            p.cam_choice.0 = index;
        } else {
            p.cam_choice.1 = index;
        }
    }
    app.view = mode.into();
    app.ego = false;
    app.sync_view_look();
    let mut camera = if mode == "free" {
        fallback
    } else {
        app.player.as_ref().unwrap().camera_look(
            mode,
            &Camera {
                fov_deg: 60.0,
                ..fallback
            },
            app.look,
            app.orbit,
        )
    };
    camera.fov_deg = (base_fov(app, &camera) * app.view_zoom.get(mode).copied().unwrap_or(1.0))
        .clamp(8.0, 120.0);
    // Explicit API writes take effect immediately; an earlier UI glide must not resume.
    app.cam_blend = Default::default();
    app.camera = Some(camera);
    app.hover_key = None;
    snapshot(app)
}

fn set(app: &mut App, args: &Value) -> Result<Value, String> {
    keys(
        args,
        &[
            "position",
            "heading_degrees",
            "pitch_degrees",
            "roll_degrees",
            "near_metres",
            "far_metres",
            "fov_degrees",
            "look_yaw_degrees",
            "look_pitch_degrees",
            "orbit_metres",
        ],
    )?;
    if args.as_object().unwrap().keys().all(|k| k == "session_id") {
        return Err("camera update is empty".into());
    }
    let fallback = app.camera.ok_or("camera is not loaded")?;
    let free = app.view == "free" && !app.ego && app.on_foot.is_none();
    let world_fields = [
        "position",
        "heading_degrees",
        "pitch_degrees",
        "roll_degrees",
        "near_metres",
        "far_metres",
    ];
    if !free && world_fields.iter().any(|k| args.get(*k).is_some()) {
        return Err(
            "world pose and clip planes require the free camera; vehicle views use look angles"
                .into(),
        );
    }
    if app.view == "foot" || app.on_foot.is_some() || app.ego {
        return Err("use the walking-driver API to control its view".into());
    }
    let mut look = current_look(app);
    for (key, slot, min, max) in [
        ("look_yaw_degrees", &mut look.0, -360.0, 360.0),
        ("look_pitch_degrees", &mut look.1, -85.0, 85.0),
    ] {
        if let Some(value) = args.get(key) {
            if free {
                return Err("the free camera uses heading_degrees and pitch_degrees".into());
            }
            *slot = bounded(value, key, min, max)?;
        }
    }
    let mut orbit = app.orbit;
    if let Some(value) = args.get("orbit_metres") {
        if app.view != "outside" {
            return Err("orbit_metres requires the outside camera".into());
        }
        orbit = bounded(
            value,
            "orbit_metres",
            crate::camera_util::ORBIT_MIN as f64,
            crate::camera_util::ORBIT_MAX as f64,
        )?;
    }
    let mut camera = if free {
        fallback
    } else {
        app.player
            .as_ref()
            .ok_or("player vehicle is not loaded")?
            .camera_look(
                &app.view,
                &Camera {
                    fov_deg: 60.0,
                    ..fallback
                },
                look,
                orbit,
            )
    };
    if let Some(value) = args.get("position") {
        camera.position = crate::plugin_api_vehicle::world_position(value)?;
    }
    if let Some(value) = args.get("heading_degrees") {
        camera.yaw = number(value, "heading_degrees")?.rem_euclid(360.0) as f32;
    }
    if let Some(value) = args.get("roll_degrees") {
        camera.roll = number(value, "roll_degrees")?.rem_euclid(360.0) as f32;
    }
    if let Some(value) = args.get("pitch_degrees") {
        camera.pitch = bounded(value, "pitch_degrees", -89.0, 89.0)?;
    }
    for (key, slot) in [
        ("near_metres", &mut camera.near),
        ("far_metres", &mut camera.far),
    ] {
        if let Some(value) = args.get(key) {
            *slot = bounded(value, key, 0.001, 1_000_000.0)?;
        }
    }
    if camera.near >= camera.far {
        return Err("near_metres must be less than far_metres".into());
    }
    let base = base_fov(app, &camera);
    let zoom = if let Some(value) = args.get("fov_degrees") {
        bounded(value, "fov_degrees", 8.0, 120.0)? / base
    } else {
        app.view_zoom.get(&app.view).copied().unwrap_or(1.0)
    };
    camera.fov_deg = (base * zoom).clamp(8.0, 120.0);
    app.sync_view_look();
    app.look = look;
    app.orbit = orbit;
    app.view_zoom.insert(app.view.clone(), zoom);
    // Explicit API writes take effect immediately; an earlier UI glide must not resume.
    app.cam_blend = Default::default();
    app.camera = Some(camera);
    app.hover_key = None;
    snapshot(app)
}

pub(crate) fn execute(
    app: &mut App,
    operation: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    Some(match operation {
        "camera.get" => keys(args, &[]).and_then(|_| snapshot(app)),
        "camera.select" => select(app, args),
        "camera.set" => set(app, args),
        _ => return None,
    })
}

/// Driver-head displacement, the actual spring state used by the driver's view.
/// It is a state write; the normal head physics continues on the next frame.
pub(crate) fn head(
    player: &mut crate::Player,
    operation: &str,
    args: &Value,
) -> Result<Value, String> {
    let fields = args.as_object().ok_or("arguments must be an object")?;
    for key in fields.keys() {
        if !["id", "vehicle_id", "generation", "session_id"].contains(&key.as_str())
            && !(operation == "vehicle.head.set"
                && ["position_body_metres", "velocity_body_metres_per_second"]
                    .contains(&key.as_str()))
        {
            return Err(format!("unsupported head argument: {key}"));
        }
    }
    if operation == "vehicle.head.set" {
        let vector = |name: &str, limit: f64| -> Result<Option<glam::Vec3>, String> {
            let Some(value) = args.get(name) else {
                return Ok(None);
            };
            let values = value
                .as_array()
                .filter(|v| v.len() == 3)
                .ok_or_else(|| format!("{name} must contain three numbers"))?;
            Ok(Some(glam::Vec3::new(
                bounded(&values[0], name, -limit, limit)?,
                bounded(&values[1], name, -limit, limit)?,
                bounded(&values[2], name, -limit, limit)?,
            )))
        };
        let position = vector("position_body_metres", 2.0)?;
        let velocity = vector("velocity_body_metres_per_second", 100.0)?;
        if position.is_none() && velocity.is_none() {
            return Err("head update is empty".into());
        }
        if let Some(position) = position {
            player.head = position;
        }
        if let Some(velocity) = velocity {
            player.head_vel = velocity;
        }
    }
    Ok(json!({"position_body_metres":player.head.to_array(),
        "velocity_body_metres_per_second":player.head_vel.to_array(),"seat_body_metres":player.seat.to_array()}))
}
