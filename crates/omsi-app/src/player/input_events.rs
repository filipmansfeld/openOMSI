//! Observations of accepted clicks; picking, occlusion and script behavior stay owned
//! by Player. Forgiving picks without an exact central ray hit have no coordinates.
use super::*;

fn coordinates(mesh: &omsi_geometry::MeshData, transform: Mat4, position: DVec3,
    origin: DVec3, dir: Vec3) -> Option<([f32; 2], [f32; 3])> {
    let origin = (origin - position).as_vec3();
    let hit = omsi_geometry::ray_mesh_hit(origin, dir, mesh, &transform)?;
    let local = transform.inverse().transform_point3(origin + dir * hit.t);
    // mesh_from_o3d swapped y/z when loading into the engine's mesh frame.
    let authored = [local.x, local.z, local.y];
    (local.is_finite() && hit.uv.is_finite()).then_some((hit.uv.to_array(), authored))
}

pub(super) fn record(vehicle: &mut omsi_sim::VehicleInstance, section: usize,
    mesh_index: usize, pressed: bool, ray: Option<(DVec3, Vec3)>) {
    let (ty, position, transform) = if section == 0 {
        (&vehicle.ty, vehicle.position, vehicle.mesh_local_transform(mesh_index))
    } else {
        let Some(part) = vehicle.trailers.get(section - 1) else { return };
        (&part.ty, part.position, part.mesh_local_transform(mesh_index))
    };
    let Some(mesh) = ty.meshes.get(mesh_index) else { return };
    let def = &ty.model.meshes[mesh.def_index];
    let Some(trigger) = def.mouse_event.clone() else { return };
    let file = def.file.clone();
    let hit = ray.and_then(|(origin, dir)| coordinates(&mesh.data, transform, position, origin, dir));
    vehicle.record_pointer_input(section, trigger, file, pressed,
        hit.map(|(uv, _)| uv), hit.map(|(_, position)| position));
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec2;
    use std::{path::PathBuf, sync::Arc};

    struct Fixture(PathBuf);
    impl Fixture {
        fn player(&self) -> Player {
            std::fs::write(self.0.join("bus.bus"), "[model]\nmodel.cfg\n[mass]\n10000\n").unwrap();
            std::fs::write(self.0.join("model.cfg"), "").unwrap();
            let mut ty = omsi_sim::VehicleType::load(&self.0, &self.0.join("bus.bus")).unwrap();
            let def = omsi_model::MeshDef { file: "panel.o3d".into(),
                mouse_event: Some("panel_press".into()), ..Default::default() };
            ty.model.meshes.push(def);
            let mesh = omsi_geometry::MeshData {
                positions: vec![Vec3::new(-1.0, -1.0, 0.0), Vec3::new(1.0, -1.0, 0.0), Vec3::new(0.0, 1.0, 0.0)],
                uvs: vec![Vec2::ZERO, Vec2::X, Vec2::Y], indices: vec![0, 1, 2],
                ranges: vec![(0, 3, 0)], ..Default::default()
            };
            ty.mesh_bounds.push(omsi_geometry::bounding_sphere(&mesh.positions));
            ty.mesh_boxes.push((Vec3::new(-1.0, -1.0, 0.0), Vec3::new(1.0, 1.0, 0.0)));
            ty.meshes.push(omsi_sim::vehicle::VehicleMesh { def_index: 0, data: mesh,
                file: self.0.join("panel.o3d"), materials: vec![], overrides: vec![],
                pivot: Mat4::IDENTITY, viewpoint: 0, skin: vec![] });
            let ty = Arc::new(ty);
            let mut vehicle = omsi_sim::VehicleInstance::new(ty.clone(), Default::default());
            vehicle.rigid = None;
            vehicle.position = DVec3::new(10.0, 20.0, 30.0);
            vehicle.mesh_transforms[0] = Mat4::from_translation(Vec3::new(2.0, 3.0, 4.0));
            let mut trailer = omsi_sim::vehicle::TrailerPart::new(
                ty.clone(), &ty, &ty.program, 0);
            trailer.position = DVec3::new(40.0, 50.0, 60.0);
            vehicle.trailers.push(trailer);
            crate::plugin_api_contract::player(vehicle)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn accepted_front_and_rear_clicks_and_releases_are_observed_without_script_triggers() {
        let root = std::env::temp_dir().join(format!("openomsi-pointer-input-{}", crate::plugin_api::random_id()));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Fixture(root);
        let mut player = fixture.player();
        let origin = DVec3::new(12.0, 23.0, 35.0);
        assert_eq!(player.click(origin, -Vec3::Z, 0.0), Some(0));
        player.release();
        let events = player.vehicle.pointer_input_events(Some(0), 64).unwrap();
        assert_eq!(events.events.len(), 2);
        let down = events.events[0];
        assert!(down.pressed);
        assert_eq!(down.trigger, "panel_press");
        assert_eq!(down.mesh, "panel.o3d");
        assert_eq!(down.section, 0);
        assert_eq!(down.mesh_position, Some([0.0, 0.0, 0.0]));
        let uv = down.uv.unwrap();
        assert!((uv[0] - 0.25).abs() < 1e-5 && (uv[1] - 0.5).abs() < 1e-5);
        assert!(!events.events[1].pressed);
        assert!(events.events[1].uv.is_none());
        assert!(events.events[1].mesh_position.is_none());
        // The physical release is observed even when the native switch stays held.
        assert_eq!(player.click(DVec3::new(40.0, 50.0, 61.0), -Vec3::Z, 0.0), Some(0));
        player.release_keeping();
        let rear = player.vehicle.pointer_input_events(Some(2), 64).unwrap();
        assert_eq!(rear.events.len(), 2);
        assert_eq!(rear.events[0].section, 1);
        assert!(rear.events[0].pressed && !rear.events[1].pressed);
        assert_eq!(rear.events[1].section, 1);
        assert_eq!(rear.next_after, 4);
        assert!(player.pressed_trailer_mesh.is_none());
    }

    #[test]
    fn exact_hits_report_authored_coordinates_and_forgiving_misses_have_no_coordinates() {
        let mesh = omsi_geometry::MeshData {
            positions: vec![Vec3::ZERO, Vec3::X, Vec3::Z],
            indices: vec![0, 1, 2], uvs: vec![Vec2::ZERO, Vec2::X, Vec2::Y],
            ..Default::default()
        };
        let transform = Mat4::from_rotation_z(0.7) * Mat4::from_translation(Vec3::new(2.0, 3.0, 4.0));
        let position = DVec3::new(1000.0, 2000.0, 3.0);
        let target = transform.transform_point3(Vec3::new(0.2, 0.0, 0.3));
        let direction = transform.transform_vector3(Vec3::Y);
        let origin = position + (target - direction * 2.0).as_dvec3();
        let (uv, authored) = coordinates(&mesh, transform, position, origin, direction).unwrap();
        assert!((uv[0] - 0.2).abs() < 1e-4 && (uv[1] - 0.3).abs() < 1e-4);
        assert!((Vec3::from_array(authored) - Vec3::new(0.2, 0.3, 0.0)).length() < 1e-4);
        assert!(coordinates(&mesh, transform, position, origin + Vec3::Z.as_dvec3() * 10.0, direction).is_none());
    }

    #[test]
    fn production_api_uses_live_identity_and_lossless_independent_cursors() {
        use clap::Parser;
        use serde_json::json;
        let root = std::env::temp_dir().join(format!("openomsi-pointer-api-{}", crate::plugin_api::random_id()));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Fixture(root);
        let mut app = crate::new_app(crate::Args::parse_from(["openomsi", "--root", "."]), Default::default());
        app.player = Some(fixture.player());
        let vehicle = crate::plugin_api::execute(&mut app, "vehicle.get", json!({}), &[]).unwrap();
        let mut args = json!({"id":vehicle["id"],"generation":vehicle["generation"]});
        let call = |app: &mut crate::App, args| crate::plugin_api::execute(app, "vehicle.input_events", args, &[]);
        let initial = call(&mut app, args.clone()).unwrap();
        assert_eq!(initial["next_after"], "0");
        assert_eq!(initial["last_sequence"], "0");
        assert!(initial["events"].as_array().unwrap().is_empty());
        let p = app.player.as_mut().unwrap();
        assert_eq!(p.click(DVec3::new(12.0, 23.0, 35.0), -Vec3::Z, 0.0), Some(0));
        p.release();
        // Synthetic script/API triggers cannot produce a physical pointer event.
        let trigger = json!({"id":vehicle["id"],"generation":vehicle["generation"],"name":"panel_press"});
        assert!(crate::plugin_api::execute(&mut app, "vehicle.trigger", trigger, &[]).is_err());
        assert!(call(&mut app, args.clone()).unwrap()["events"].as_array().unwrap().is_empty());
        args["after"] = json!("0");
        args["limit"] = json!(1);
        let first = call(&mut app, args.clone()).unwrap();
        assert_eq!(first["events"][0]["sequence"], "1");
        assert_eq!(first["events"][0]["kind"], "down");
        assert_eq!(first["next_after"], "1");
        assert_eq!(first["last_sequence"], "2");
        assert_eq!(call(&mut app, args.clone()).unwrap(), first);
        args["after"] = first["next_after"].clone();
        let second = call(&mut app, args.clone()).unwrap();
        assert_eq!(second["events"][0]["kind"], "up");
        assert_eq!(second["next_after"], "2");
        for invalid in [json!("-1"), json!(0), json!("3"), json!("18446744073709551616"), json!("+0"), json!(null)] {
            let mut bad = args.clone(); bad["after"] = invalid;
            assert!(call(&mut app, bad).is_err());
        }
        for invalid in [json!(0), json!(65), json!(1.5), json!("1")] {
            let mut bad = args.clone(); bad["limit"] = invalid;
            assert!(call(&mut app, bad).is_err());
        }
        assert!(call(&mut app, json!({"id":vehicle["id"]})).is_err());
        assert!(call(&mut app, json!({"generation":vehicle["generation"]})).is_err());
        let snapshot = crate::plugin_api::snapshot(&mut app);
        assert!(snapshot["capabilities"].as_array().unwrap().iter().any(|v| v == "vehicle_input_events"));
        assert!(snapshot["native_operations"].as_array().unwrap().iter().any(|v| v == "vehicle.input_events"));
        app.player = None;
        crate::plugin_api::snapshot(&mut app);
        app.player = Some(fixture.player());
        assert!(call(&mut app, args).is_err(), "replacement at the same UID must reject the old generation");
        let replacement = crate::plugin_api::execute(&mut app, "vehicle.get", json!({}), &[]).unwrap();
        let replacement_args = json!({"id":replacement["id"],"generation":replacement["generation"]});
        assert_ne!(replacement["generation"], vehicle["generation"]);
        assert_eq!(call(&mut app, replacement_args).unwrap()["last_sequence"], "0");
    }
}
