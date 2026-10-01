//! Native vehicle controls, pose and model/physics inspection. Identity validation is
//! performed by the shared dispatcher before this module receives a vehicle.
use glam::{DVec3, Quat, Vec3};
use omsi_sim::{physics::Controls, VehicleInstance};
use serde_json::{json, Value};

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        if !["id", "vehicle_id", "session_id", "generation"].contains(&key.as_str())
            && !allowed.contains(&key.as_str())
        {
            return Err(format!("unsupported argument: {key}"));
        }
    }
    Ok(())
}

fn number(value: &Value, name: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))
}

pub(crate) fn world_position(value: &Value) -> Result<DVec3, String> {
    let p = value
        .as_object()
        .ok_or("position must contain east, north and up")?;
    if p.len() != 3 || !["east", "north", "up"].iter().all(|k| p.contains_key(*k)) {
        return Err("position must contain exactly east, north and up".into());
    }
    let position = DVec3::new(
        number(&p["east"], "east")?,
        number(&p["north"], "north")?,
        number(&p["up"], "up")?,
    );
    // Tiling indexes use i32 and many local collision calculations use f32.
    // Keep a margin at the indexing boundary; reject astronomical finite values.
    let horizontal = ((i32::MAX as f64 - 1024.0) * omsi_map::tile_size()).min(1_000_000_000.0);
    if position.x.abs() > horizontal
        || position.y.abs() > horizontal
        || position.z.abs() > 1_000_000.0
    {
        return Err("position exceeds the supported map coordinate range".into());
    }
    Ok(position)
}

fn controls(current: Controls, args: &Value) -> Result<Controls, String> {
    keys(args, &["controls"])?;
    let values = args
        .get("controls")
        .and_then(Value::as_object)
        .filter(|c| !c.is_empty())
        .ok_or("controls must be a nonempty object")?;
    let mut next = current;
    for (name, value) in values {
        let n = number(value, name)?;
        let (slot, lo) = match name.as_str() {
            "throttle" => (&mut next.throttle, 0.0),
            "brake" => (&mut next.brake, 0.0),
            "clutch" => (&mut next.clutch, 0.0),
            "steering" => (&mut next.steering, -1.0),
            _ => return Err(format!("unknown control: {name}")),
        };
        if !(lo..=1.0).contains(&n) {
            return Err(format!("{name} must be in {lo}..1"));
        }
        *slot = n as f32;
    }
    Ok(next)
}

struct PoseUpdate {
    position: DVec3,
    heading: f64,
    pitch: f32,
    bank: f32,
    preserve_velocity: bool,
}

fn pose(
    position: DVec3,
    heading: f64,
    pitch: f32,
    bank: f32,
    args: &Value,
) -> Result<PoseUpdate, String> {
    keys(
        args,
        &[
            "position",
            "heading_degrees",
            "pitch_degrees",
            "bank_degrees",
            "preserve_velocity",
        ],
    )?;
    if ![
        "position",
        "heading_degrees",
        "pitch_degrees",
        "bank_degrees",
    ]
    .iter()
    .any(|k| args.get(*k).is_some())
    {
        return Err("set_pose needs a position or angle".into());
    }
    let mut out = PoseUpdate {
        position,
        heading,
        pitch,
        bank,
        preserve_velocity: true,
    };
    if let Some(value) = args.get("position") {
        out.position = world_position(value)?;
    }
    if let Some(value) = args.get("heading_degrees") {
        out.heading = number(value, "heading_degrees")?.rem_euclid(360.0);
    }
    for (key, slot) in [
        ("pitch_degrees", &mut out.pitch),
        ("bank_degrees", &mut out.bank),
    ] {
        if let Some(value) = args.get(key) {
            let angle = number(value, key)?.rem_euclid(360.0);
            *slot = (if angle >= 180.0 { angle - 360.0 } else { angle }) as f32;
        }
    }
    if let Some(value) = args.get("preserve_velocity") {
        out.preserve_velocity = value
            .as_bool()
            .ok_or("preserve_velocity must be a boolean")?;
    }
    Ok(out)
}

fn controls_value(c: Controls) -> Value {
    json!({"throttle":c.throttle,"brake":c.brake,"clutch":c.clutch,"steering":c.steering})
}

fn vector(value: &Value, name: &str) -> Result<Vec3, String> {
    let a = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| format!("{name} must contain three numbers"))?;
    let result = Vec3::new(
        number(&a[0], name)? as f32,
        number(&a[1], name)? as f32,
        number(&a[2], name)? as f32,
    );
    if !result.is_finite() {
        return Err(format!("{name} exceeds the native numeric range"));
    }
    let limit = if matches!(name, "velocity_world" | "angular_velocity_body") {
        10_000.0
    } else {
        1_000_000_000_000.0
    };
    if result.abs().max_element() > limit {
        return Err(format!(
            "{name} exceeds the supported component magnitude {limit}"
        ));
    }
    Ok(result)
}

fn motion(
    r: &mut omsi_sim::rigid::RigidBody,
    operation: &str,
    args: &Value,
) -> Result<Value, String> {
    let (first, second) = if operation == "vehicle.set_velocity" {
        ("velocity_world", "angular_velocity_body")
    } else {
        ("force_world", "torque_body")
    };
    keys(args, &[first, second])?;
    let a = args.get(first).map(|v| vector(v, first)).transpose()?;
    let b = args.get(second).map(|v| vector(v, second)).transpose()?;
    if a.is_none() && b.is_none() {
        return Err(format!("{first} or {second} is required"));
    }
    if operation == "vehicle.set_velocity" {
        if let Some(v) = a {
            r.velocity = v;
        }
        if let Some(v) = b {
            r.omega = v;
        }
        Ok(
            json!({"velocity_world":r.velocity.to_array(),"angular_velocity_body":r.omega.to_array()}),
        )
    } else {
        let force = r.external_force_world + a.unwrap_or(Vec3::ZERO);
        let torque = r.external_torque_body + b.unwrap_or(Vec3::ZERO);
        if !force.is_finite()
            || !torque.is_finite()
            || force.abs().max_element() > 1e12
            || torque.abs().max_element() > 1e12
        {
            return Err("accumulated load exceeds the supported component magnitude 1e12".into());
        }
        r.external_force_world = force;
        r.external_torque_body = torque;
        Ok(json!({"pending_force_world":force.to_array(),"pending_torque_body":torque.to_array()}))
    }
}

pub(crate) fn execute(
    v: &mut VehicleInstance,
    operation: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    Some(match operation {
        "vehicle.set_velocity" | "vehicle.apply_force" => {
            let Some(r) = v.rigid.as_mut() else {
                return Some(Err("this operation requires rigid-body physics".into()));
            };
            let result = motion(r, operation, args);
            if result.is_ok() {
                v.physics.speed = r.forward_speed();
            }
            result
        }
        "vehicle.set_controls" => controls(
            v.queued_native_controls().unwrap_or(v.physics.controls),
            args,
        )
        .map(|c| {
            v.queue_native_controls(c);
            json!({"queued":true,"controls":controls_value(c)})
        }),
        "vehicle.set_pose" => pose(v.position, v.heading, v.pitch, v.bank, args).map(|p| {
            let orientation = Quat::from_rotation_z((-p.heading).to_radians() as f32)
                * Quat::from_rotation_x(p.pitch.to_radians())
                * Quat::from_rotation_y(p.bank.to_radians());
            v.position = p.position;
            v.heading = p.heading;
            v.pitch = p.pitch;
            v.bank = p.bank;
            if let Some(rb) = v.rigid.as_mut() {
                rb.orientation = orientation;
                rb.position = p.position + orientation.mul_vec3(rb.cog).as_dvec3();
                rb.spawned_inside = None;
                for wheel in &mut rb.wheels {
                    wheel.ground_seen = false;
                    wheel.walls.clear();
                    wheel.touch = None;
                }
                if !p.preserve_velocity {
                    rb.velocity = Vec3::ZERO;
                    rb.omega = Vec3::ZERO;
                }
                v.physics.speed = rb.forward_speed();
            } else if !p.preserve_velocity {
                v.physics.speed = 0.0;
            }
            for part in &mut v.trailers {
                part.realign();
            }
            v.retrail(0.0, &|_| None);
            v.update_ground_probe();
            json!({"position":{"east":v.position.x,"north":v.position.y,"up":v.position.z},
                "heading_degrees":v.heading,"pitch_degrees":v.pitch,"bank_degrees":v.bank})
        }),
        "vehicle.physics" => {
            if let Err(e) = keys(args, &[]) {
                return Some(Err(e));
            }
            let p = &v.physics;
            let wheels: Vec<_> = p.wheels.iter().enumerate().flat_map(|(axle, pair)| pair.iter().enumerate().map(move |(side, w)|
                json!({"axle":axle,"side":side,"long":w.long,"lat":w.lat,"radius":w.radius,
                    "driven":w.driven,"rotation_degrees":w.rotation_deg,"rpm":w.rpm,"suspension":w.suspension}))).collect();
            let rigid = v.rigid.as_ref().map(|r| json!({"mass_kg":r.mass,"inertia":r.inertia.to_array(),
                "center_of_gravity":r.cog.to_array(),"velocity_world":r.velocity.to_array(),
                "angular_velocity_body":r.omega.to_array(),"orientation_xyzw":r.orientation.to_array(),
                "acceleration_body":r.accel_body.to_array(),"friction":r.friction,
                "holding":r.holding,"body_frequency":r.body_freq,"kinetic_energy":r.kinetic_energy(),
                "pending_force_world":r.external_force_world.to_array(),"pending_torque_body":r.external_torque_body.to_array(),
                "wheels":r.wheels.iter().map(|w|json!({"spin_radians_per_second":w.spin,"slipping":w.slipping,"locked":w.locked,
                    "attach":w.attach.to_array(),"radius":w.radius,"spring":w.spring,"damper":w.damper,"compression":w.compression,
                    "compression_rate":w.compression_rate,"on_ground":w.on_ground,"load_newtons":w.load,"rpm":w.rpm})).collect::<Vec<_>>()}));
            Ok(
                json!({"mode":if rigid.is_some(){"rigid"}else{"simple"},"mass_kg":p.mass_kg,
                "rolling_resistance":p.rolling_resistance,"inv_min_turn_radius":p.inv_min_turn_radius,
                "rotation_point_long":p.rot_pnt_long,"wheelbase":p.wheelbase,"speed_metres_per_second":p.speed,
                "acceleration":p.accel.to_array(),"steer_degrees":p.steer_deg,"max_steer_degrees":p.max_steer_deg,
                "steer_rate":p.steer_rate,"controls":controls_value(p.controls),"pending_controls":v.queued_native_controls().map(controls_value),"wheels":wheels,"rigid":rigid}),
            )
        }
        "vehicle.model" => model(v, args),
        "vehicle.script" => script(v, args),
        "vehicle.curve" => curve(v, args),
        _ => return None,
    })
}

fn curve(v: &VehicleInstance, args: &Value) -> Result<Value, String> {
    keys(args, &["name", "x"])?;
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("curve name is required")?;
    let x = number(args.get("x").ok_or("x is required")?, "x")? as f32;
    if !x.is_finite() {
        return Err("x exceeds the native numeric range".into());
    }
    let curve =
        v.ty.program
            .curves
            .iter()
            .rev()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or("unknown script curve")?;
    let value = curve.eval(x);
    if !value.is_finite() {
        return Err("script curve produced a nonfinite value".into());
    }
    Ok(json!({"name":curve.name,"x":x,"value":value}))
}

fn script(v: &VehicleInstance, args: &Value) -> Result<Value, String> {
    keys(args, &["kind", "offset", "limit"])?;
    let kind = args
        .get("kind")
        .map(|v| v.as_str().ok_or("kind must be a string"))
        .transpose()?
        .unwrap_or("metadata");
    let p = &v.ty.program;
    if kind == "metadata" {
        let d = &v.ty.def;
        return Ok(
            json!({"scripts":d.scripts.scripts.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>(),
            "variable_lists":d.scripts.varlists.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>(),
            "string_variable_lists":d.scripts.stringvarlists.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>(),
            "constant_files":d.scripts.constfiles.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>(),
            "curve_count":p.curves.len(),"constant_count":p.constants().count(),"block_count":p.blocks.len(),
            "triggers":p.trigger_names(),"init_blocks":p.init,"frame_blocks":p.frame,"ai_frame_blocks":p.frame_ai,
            "errors":p.errors.iter().map(|e|json!({"file_name":e.file.to_string_lossy(),"line":e.line,"message":e.message})).collect::<Vec<_>>()}),
        );
    }
    let offset = usize::try_from(
        args.get("offset")
            .map(|v| v.as_u64().ok_or("offset must be an integer"))
            .transpose()?
            .unwrap_or(0),
    )
    .map_err(|_| "offset is too large")?;
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().ok_or("limit must be an integer"))
        .transpose()?
        .unwrap_or(64);
    if !(1..=256).contains(&limit) {
        return Err("limit must be in 1..256".into());
    }
    let rows: Vec<_> = match kind {
        "constants" => {
            let values: std::collections::BTreeMap<_, _> = p.constants().collect();
            values
                .into_iter()
                .map(|(name, value)| json!({"name":name,"value":value}))
                .collect()
        }
        "curves" => p
            .curves
            .iter()
            .enumerate()
            .map(|(index, c)| json!({"index":index,"name":c.name,"points":c.points}))
            .collect(),
        "blocks" => p
            .blocks
            .iter()
            .enumerate()
            .map(|(index, b)| {
                json!({"index":index,"name":b.name,
            "file_name":b.file.to_string_lossy(),"line":b.line,"instruction_count":b.ops.len()})
            })
            .collect(),
        _ => return Err("kind must be metadata, constants, curves or blocks".into()),
    };
    if offset > rows.len() {
        return Err("offset is beyond this script collection".into());
    }
    let end = offset.saturating_add(limit as usize).min(rows.len());
    Ok(
        json!({"items":rows[offset..end],"total":rows.len(),"next_offset":if end<rows.len(){Some(end)}else{None}}),
    )
}

fn camera(c: &omsi_vehicle::Camera) -> Value {
    json!({"position_model":c.pos,"distance":c.dist,"fov":c.fov,"yaw":c.yaw,"pitch":c.pitch,"extra":c.extra})
}

fn model(v: &VehicleInstance, args: &Value) -> Result<Value, String> {
    keys(args, &["offset", "limit"])?;
    let offset = args
        .get("offset")
        .map(|v| v.as_u64().ok_or("offset must be a nonnegative integer"))
        .transpose()?
        .unwrap_or(0);
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().ok_or("limit must be a positive integer"))
        .transpose()?
        .unwrap_or(64);
    if limit == 0 || limit > 256 {
        return Err("limit must be in 1..256".into());
    }
    let offset = usize::try_from(offset).map_err(|_| "offset is too large")?;
    let d = &v.ty.def;
    let meshes: Vec<_> = v.ty.meshes.iter().enumerate().skip(offset).take(limit as usize).map(|(index, m)| {
        let materials: Vec<_> = m.materials.iter().map(|s| json!({"texture":s.texture,"diffuse":s.diffuse,
            "specular":s.specular,"emissive":s.emissive,"specular_power":s.specular_power})).collect();
        json!({"index":index,"definition_index":m.def_index,"file_name":m.file.to_string_lossy(),
            "vertex_count":m.data.positions.len(),"index_count":m.data.indices.len(),
            "viewpoint":m.viewpoint,"pivot":m.pivot.to_cols_array(),"materials":materials,
            "local_transform":v.mesh_transforms.get(index).map(|t|t.to_cols_array()),
            "visible":v.mesh_props.get(index).map(|p|p.visible)})
    }).collect();
    let next = offset.saturating_add(meshes.len());
    Ok(
        json!({"file_name":d.path.to_string_lossy(),"manufacturer":d.manufacturer,"type_name":d.type_name,
        "description":d.description,"bounding_box":d.bounding_box,"definition_mass":d.mass,
        "center_of_gravity":d.cog,"center_of_gravity_height":d.cog_height,"moment_of_inertia":d.moment_of_inertia,
        "cameras_driver":d.cameras_driver.iter().map(camera).collect::<Vec<_>>(),
        "cameras_passenger":d.cameras_pax.iter().map(camera).collect::<Vec<_>>(),
        "cameras_reflection":d.cameras_reflexion.iter().map(camera).collect::<Vec<_>>(),
        "mesh_count":v.ty.meshes.len(),"meshes":meshes,
        "next_offset":if next<v.ty.meshes.len(){Some(next)}else{None},
        "coupled_parts":v.trailers.iter().enumerate().map(|(index,p)| json!({"section":index+1,
            "file_name":p.ty.def.path.to_string_lossy(),"position":p.position.to_array(),"heading_degrees":p.heading})).collect::<Vec<_>>()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_act_on_the_solver_and_are_consumed_once() {
        let def = omsi_vehicle::Vehicle::default();
        let mut r = omsi_sim::rigid::RigidBody::from_definition(&def, &[]);
        // A body in free fall avoids contact forces; the test observes the real solver.
        let force = r.mass * 100.0;
        motion(
            &mut r,
            "vehicle.apply_force",
            &json!({"force_world":[force,0,0]}),
        )
        .unwrap();
        motion(
            &mut r,
            "vehicle.apply_force",
            &json!({"force_world":[force,0,0]}),
        )
        .unwrap();
        let pending = r.external_force_world;
        assert!(motion(
            &mut r,
            "vehicle.apply_force",
            &json!({"force_world":[1e30,0,0]})
        )
        .is_err());
        assert_eq!(r.external_force_world, pending);
        assert!(motion(
            &mut r,
            "vehicle.set_velocity",
            &json!({"velocity_world":[1,2,3],"angular_velocity_body":[1,2]})
        )
        .is_err());
        assert_eq!(r.velocity, Vec3::ZERO);
        assert_eq!(r.external_force_world, pending);
        let air = |_, _, _| omsi_sim::rigid::GroundProbe {
            below: None,
            above: None,
        };
        r.step(0.001, 0.0, &[], 0.0, &air);
        assert!((r.velocity.x - 0.2).abs() < 0.001, "{:?}", r.velocity);
        assert_eq!(r.external_force_world, Vec3::ZERO);
        assert_eq!(r.external_torque_body, Vec3::ZERO);
        let velocity = r.velocity.x;
        r.step(0.001, 0.0, &[], 0.0, &air);
        assert!(r.velocity.x <= velocity);
    }
    #[test]
    fn controls_validate_the_entire_update_before_mutation() {
        let old = Controls {
            throttle: 0.2,
            brake: 0.3,
            clutch: 0.4,
            steering: -0.5,
        };
        let next = controls(old, &json!({"controls":{"brake":0.7,"steering":1.0}})).unwrap();
        assert_eq!(
            (next.throttle, next.brake, next.clutch, next.steering),
            (0.2, 0.7, 0.4, 1.0)
        );
        assert!(controls(old, &json!({"controls":{"brake":0.9,"steering":1.1}})).is_err());
        assert!(controls(old, &json!({"controls":{"brake":0.9,"unknown":1}})).is_err());
        assert!(controls(old, &json!({"controls":{"brake":0.9},"ignored_typo":1})).is_err());
        assert_eq!(old.brake, 0.3);
    }
    #[test]
    fn pose_rejects_partial_vectors_and_preserves_velocity_by_default() {
        let old = DVec3::new(10.0, 20.0, 30.0);
        let value = pose(old, 0.0, 0.0, 0.0, &json!({"heading_degrees":-90.0})).unwrap();
        assert_eq!(value.heading, 270.0);
        assert_eq!(value.position, old);
        assert!(value.preserve_velocity);
        let angles = pose(
            old,
            0.0,
            0.0,
            0.0,
            &json!({"pitch_degrees":-5,"bank_degrees":-7}),
        )
        .unwrap();
        assert_eq!((angles.pitch, angles.bank), (-5.0, -7.0));
        assert!(pose(
            old,
            0.0,
            0.0,
            0.0,
            &json!({"position":{"east":1e300,"north":0,"up":0}})
        )
        .is_err());
        assert!(pose(
            old,
            0.0,
            0.0,
            0.0,
            &json!({"position":{"east":1,"north":2}})
        )
        .is_err());
        assert!(pose(
            old,
            0.0,
            0.0,
            0.0,
            &json!({"heading_degrees":90,"preserve_velocity":"false"})
        )
        .is_err());
    }
}
