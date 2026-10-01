//! Instance-local physical coefficients. Changes go into the actual solver and
//! its simple-physics mirror, never into another instance's shared vehicle type.
use glam::Vec3;
use omsi_sim::{rigid::RigidBody, VehicleInstance};
use serde_json::{json, Value};

fn scalar(value: &Value, name: &str, min: f32, max: f32) -> Result<f32, String> {
    let value = value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))?;
    if value < min as f64 || value > max as f64 {
        return Err(format!("{name} must be in {min}..{max}"));
    }
    Ok(value as f32)
}

fn vector(value: &Value, name: &str, min: f32, max: f32) -> Result<Vec3, String> {
    let v = value
        .as_array()
        .filter(|v| v.len() == 3)
        .ok_or_else(|| format!("{name} must contain three numbers"))?;
    Ok(Vec3::new(
        scalar(&v[0], name, min, max)?,
        scalar(&v[1], name, min, max)?,
        scalar(&v[2], name, min, max)?,
    ))
}

fn snapshot(r: &RigidBody) -> Value {
    json!({"mass_kg":r.mass,"inertia_body_kg_m2":r.inertia.to_array(),
        "center_of_gravity_body_metres":r.cog.to_array(),"rolling_resistance_newtons":r.rolling_resistance,
        "rotation_point_long_metres":r.rot_pnt_long,"inverse_min_turn_radius":r.inv_min_turn_radius,
        "max_steer_degrees":r.max_steer_deg,"body_frequency":r.body_freq,
        "wheels":r.wheels.iter().enumerate().map(|(index,w)|json!({"index":index,
            "axle":r.wheel_axle.get(index),"side":index%2,
            "attach_body_metres":w.attach.to_array(),"force_lever_metres":w.lever,
            "radius_metres":w.radius,"driven":w.driven,"steered":w.steered,
            "inverse_inertia":w.inertia_inv,"spring_newtons_per_metre":w.spring,
            "damper_newton_seconds_per_metre":w.damper,"maximum_force_newtons":w.max_force,
            "tyre_stiffness_newtons_per_metre":w.tyre_k,"tyre_damping_newton_seconds_per_metre":w.tyre_c,
            "rest_load_newtons":w.rest_load})).collect::<Vec<_>>()})
}

/// Validate the entire request against a copy before publishing any change.
fn prepare(current: &RigidBody, values: &Value) -> Result<RigidBody, String> {
    let values = values
        .as_object()
        .filter(|v| !v.is_empty())
        .ok_or("values must be a nonempty object")?;
    let mut next = current.clone();
    for (name, value) in values {
        match name.as_str() {
            "mass_kg" => next.mass = scalar(value, name, 500.0, 1_000_000.0)?,
            "inertia_body_kg_m2" => {
                let inertia = vector(value, name, 1.0, 1e10)?;
                let [x, y, z] = inertia.as_dvec3().to_array();
                if x > y + z || y > x + z || z > x + y {
                    return Err(
                        "physical inertia must satisfy all three triangle inequalities".into(),
                    );
                }
                if inertia.max_element() / inertia.min_element() > 1000.0 {
                    return Err(
                        "inertia component ratio must not exceed 1000 for the native solver".into(),
                    );
                }
                next.inertia = inertia;
            }
            "center_of_gravity_body_metres" => next.cog = vector(value, name, -100.0, 100.0)?,
            "rolling_resistance_newtons" => {
                next.rolling_resistance = scalar(value, name, 0.0, 1e8)?
            }
            "rotation_point_long_metres" => next.rot_pnt_long = scalar(value, name, -100.0, 100.0)?,
            "inverse_min_turn_radius" => next.inv_min_turn_radius = scalar(value, name, 0.0, 10.0)?,
            "wheels" => {
                let wheels = value
                    .as_array()
                    .filter(|w| !w.is_empty() && w.len() <= 128)
                    .ok_or("wheels must be an array of 1..128 indexed updates")?;
                let mut seen = std::collections::HashSet::new();
                for update in wheels {
                    let update = update
                        .as_object()
                        .filter(|w| w.len() > 1)
                        .ok_or("each wheel update needs index and at least one property")?;
                    let index = update
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|i| usize::try_from(i).ok())
                        .ok_or("wheel index must be a nonnegative integer")?;
                    if !seen.insert(index) {
                        return Err("duplicate wheel index in one update".into());
                    }
                    let wheel = next
                        .wheels
                        .get_mut(index)
                        .ok_or("wheel index is unavailable")?;
                    for (field, value) in update {
                        match field.as_str() {
                            "index" => {}
                            "attach_body_metres" => {
                                wheel.attach = vector(value, field, -100.0, 100.0)?
                            }
                            "force_lever_metres" => {
                                wheel.lever = scalar(value, field, -20.0, 20.0)?
                            }
                            "radius_metres" => wheel.radius = scalar(value, field, 0.05, 10.0)?,
                            "inverse_inertia" => {
                                wheel.inertia_inv = scalar(value, field, 1e-8, 10.0)?
                            }
                            "spring_newtons_per_metre" => {
                                wheel.spring = scalar(value, field, 1.0, 1e7)?
                            }
                            "damper_newton_seconds_per_metre" => {
                                wheel.damper = scalar(value, field, 0.0, 1e6)?
                            }
                            "maximum_force_newtons" => {
                                wheel.max_force = scalar(value, field, 1.0, 1e8)?
                            }
                            "tyre_stiffness_newtons_per_metre" => {
                                wheel.tyre_k = scalar(value, field, 1.0, 1e8)?
                            }
                            "tyre_damping_newton_seconds_per_metre" => {
                                wheel.tyre_c = scalar(value, field, 0.0, 1e6)?
                            }
                            "driven" => {
                                wheel.driven = value.as_bool().ok_or("driven must be a boolean")?
                            }
                            _ => return Err(format!("unsupported wheel property: {field}")),
                        }
                    }
                }
            }
            _ => return Err(format!("unsupported physics property: {name}")),
        }
    }
    // Keep the visible model origin and motion. Moving the CoG does not teleport
    // the mesh, or erase the velocity/impulse already queued by another command.
    next.position = current.origin() + next.orientation.mul_vec3(next.cog).as_dvec3();
    next.body_freq = (next.wheels.iter().map(|w| w.spring).sum::<f32>() / next.mass).sqrt();
    if !next.wheels.is_empty() {
        let front = next
            .wheels
            .iter()
            .map(|w| w.attach.y)
            .fold(f32::MIN, f32::max);
        next.max_steer_deg = ((front - next.rot_pnt_long).abs().max(1.0)
            * next.inv_min_turn_radius)
            .atan()
            .to_degrees();
    }
    // Match the solver's existing longitudinal static-load model, now using live
    // wheel geometry instead of the immutable authored axles.
    let n = next.wheels.len().max(1) as f32;
    let sy = next.wheels.iter().map(|w| w.attach.y).sum::<f32>();
    let syy = next
        .wheels
        .iter()
        .map(|w| w.attach.y * w.attach.y)
        .sum::<f32>();
    let weight = next.mass * 9.81;
    let det = n * syy - sy * sy;
    let (a, b) = if det.abs() > 1e-6 {
        (
            (weight * syy - sy * weight * next.cog.y) / det,
            (n * weight * next.cog.y - sy * weight) / det,
        )
    } else {
        (weight / n, 0.0)
    };
    for (i, wheel) in next.wheels.iter_mut().enumerate() {
        wheel.rest_load = (a + b * wheel.attach.y).max(weight / n * 0.2);
        let old = &current.wheels[i];
        if wheel.attach != old.attach || wheel.radius != old.radius {
            wheel.ground_seen = false;
            wheel.touch = None;
            wheel.walls.clear();
        }
    }
    Ok(next)
}

pub(crate) fn execute(
    vehicle: &mut VehicleInstance,
    operation: &str,
    args: &Value,
) -> Result<Value, String> {
    let fields = args.as_object().ok_or("arguments must be an object")?;
    for key in fields.keys() {
        if !["id", "vehicle_id", "generation", "session_id"].contains(&key.as_str())
            && !(operation == "vehicle.physics.configure" && key == "values")
        {
            return Err(format!("unsupported argument: {key}"));
        }
    }
    let current = vehicle
        .rigid
        .as_ref()
        .ok_or("physical coefficient operations require rigid-body physics")?;
    if operation == "vehicle.physics.parameters" {
        return Ok(snapshot(current));
    }
    let next = prepare(current, args.get("values").ok_or("values are required")?)?;
    // These mirrors are consumed by speed/kinematic fallback and script telemetry.
    // The immutable definition and other vehicles using it stay independent.
    mirror_parameters(&mut vehicle.physics, &next);
    let result = snapshot(&next);
    vehicle.rigid = Some(next);
    Ok(result)
}

fn mirror_parameters(p: &mut omsi_sim::physics::VehiclePhysics, next: &RigidBody) {
    p.mass_kg = next.mass;
    p.rolling_resistance = next.rolling_resistance;
    p.rot_pnt_long = next.rot_pnt_long;
    p.inv_min_turn_radius = next.inv_min_turn_radius;
    p.max_steer_deg = next.max_steer_deg;
    for (index, w) in next.wheels.iter().enumerate() {
        if let Some(pair) = next
            .wheel_axle
            .get(index)
            .and_then(|a| p.wheels.get_mut(*a))
        {
            let dst = &mut pair[index % 2];
            dst.long = w.attach.y;
            // Simple physics probes at the outside of the tyre band; rigid
            // contact sits at its centre and the force lever reaches outside.
            dst.lat = w.attach.x + w.lever;
            dst.radius = w.radius;
            dst.driven = w.driven;
        }
    }
    if !p.wheels.is_empty() {
        let front = p.wheels.iter().map(|w| w[0].long).fold(f32::MIN, f32::max);
        let rear = p.wheels.iter().map(|w| w[0].long).fold(f32::MAX, f32::min);
        p.wheelbase = (front - rear).abs().max(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coefficient_updates_change_solver_acceleration_and_preserve_motion() {
        let mut current = RigidBody::from_definition(&omsi_vehicle::Vehicle::default(), &[]);
        current.position = glam::DVec3::new(12.0, 34.0, 56.0);
        current.velocity = Vec3::new(1.0, 2.0, 3.0);
        current.omega = Vec3::new(0.1, 0.2, 0.3);
        current.external_force_world = Vec3::Z * current.mass * 10.0;
        let next = prepare(
            &current,
            &json!({"mass_kg":current.mass*2.0,"center_of_gravity_body_metres":[1,2,3]}),
        )
        .unwrap();
        assert!((next.origin() - current.origin()).length() < 1e-6);
        assert_eq!(next.velocity, current.velocity);
        assert_eq!(next.omega, current.omega);
        assert_eq!(next.external_force_world, current.external_force_world);
        // Same force, twice the mass: the real free-fall solver changes vertical
        // acceleration by exactly half the injected acceleration.
        let mut lighter = current.clone();
        let mut heavier = next;
        let air = |_, _, _| omsi_sim::rigid::GroundProbe {
            below: None,
            above: None,
        };
        lighter.step(0.01, 0.0, &[], 0.0, &air);
        heavier.step(0.01, 0.0, &[], 0.0, &air);
        assert!((lighter.velocity.z - heavier.velocity.z - 0.05).abs() < 1e-4);
    }

    #[test]
    fn wheel_updates_recompute_loads_and_reject_a_bad_batch_atomically() {
        let mut def = omsi_vehicle::Vehicle::default();
        def.mass = 10000.0;
        def.axles = vec![omsi_vehicle::Axle {
            long: 3.0,
            max_width: 2.0,
            min_width: 1.5,
            wheel_diameter: 1.0,
            spring: 200.0,
            max_force: 100.0,
            damper: 20.0,
            driven: true,
            inertia_inv: 0.002,
        }];
        let current = RigidBody::from_definition(&def, &[]);
        let mut mirror = omsi_sim::physics::VehiclePhysics::from_definition(&def);
        let outer = [mirror.wheels[0][0].lat, mirror.wheels[0][1].lat];
        let mass_only = prepare(&current, &json!({"mass_kg":20000})).unwrap();
        mirror_parameters(&mut mirror, &mass_only);
        assert_eq!([mirror.wheels[0][0].lat, mirror.wheels[0][1].lat], outer);
        let next = prepare(
            &current,
            &json!({"mass_kg":20000,"wheels":[{"index":0,"spring_newtons_per_metre":400000}]}),
        )
        .unwrap();
        assert_eq!(next.wheels[0].spring, 400000.0);
        assert_eq!(next.wheels[0].rest_load, current.wheels[0].rest_load * 2.0);
        assert!((next.body_freq - (600000.0f32 / 20000.0).sqrt()).abs() < 1e-5);
        let before = snapshot(&current);
        assert!(prepare(
            &current,
            &json!({"mass_kg":20000,"wheels":[{"index":0,"radius_metres":0}]})
        )
        .is_err());
        assert!(prepare(
            &current,
            &json!({"wheels":[{"index":0,"driven":false},{"index":0,"driven":true}]})
        )
        .is_err());
        assert_eq!(snapshot(&current), before);
    }

    #[test]
    fn inertia_bounds_reject_unphysical_gyro_and_remain_finite_in_solver() {
        let mut current = RigidBody::from_definition(&omsi_vehicle::Vehicle::default(), &[]);
        current.omega = Vec3::new(0.1, 0.2, 0.3);
        assert!(prepare(&current, &json!({"inertia_body_kg_m2":[1,1e10,1]})).is_err());
        assert!(prepare(&current, &json!({"inertia_body_kg_m2":[1,1e10,1e10]})).is_err());
        let air = |_, _, _| omsi_sim::rigid::GroundProbe {
            below: None,
            above: None,
        };
        for inertia in [[1.0, 1000.0, 1000.0], [1e10, 1e7, 1e10], [1e10, 1e10, 1e10]] {
            let mut next = prepare(&current, &json!({"inertia_body_kg_m2":inertia})).unwrap();
            for _ in 0..300 {
                next.step(1.0 / 60.0, 0.0, &[], 0.0, &air);
            }
            assert!(
                next.orientation.is_finite() && next.omega.is_finite() && next.position.is_finite()
            );
            assert!(next.omega.length() < 1.0);
        }
    }
}
