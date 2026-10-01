//! Variable declarations are runtime state even when a scenery object has no OSC script.
use super::*;
use clap::Parser;
use serde_json::json;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "openomsi-scenery-variables-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Ok(path), Ok(root)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
            if path.starts_with(&root) && path != root {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

#[test]
fn declared_scenery_variables_survive_loading_and_native_api_updates_without_scripts() {
    let fixture = Fixture::new();
    fixture.write(
        "global.cfg",
        "[name]\nVariable state regression\n[map]\n0\n0\ntile_0_0.map\n",
    );
    fixture.write("vars.txt", "enabled\n");
    fixture.write("strings.txt", "label\ntarget\n");
    let declarations = "[varnamelist]\n1\nvars.txt\n[stringvarnamelist]\n1\nstrings.txt\n";
    let mesh = "[mesh]\nindicator.x\n";
    fixture.write("plain.sco", mesh);
    fixture.write("state.sco", &format!(
        "{mesh}{declarations}[traffic_lights_group]\n40\n[traffic_light]\nOutbound\n[phase]\n0\n20\n[phase]\n6\n20\n[traffic_light]\nReturn\n[phase]\n6\n20\n[phase]\n0\n20\n",
    ));
    fixture.write(
        "indicator.sco",
        &format!(
        "{mesh}{declarations}[matl_change]\nbase.png\n0\nenabled\n[matl_item]\n[matl_alpha]\n2\n",
    ),
    );
    fixture.write("helper.sco", declarations);
    fixture.write("indicator.x", r#"xof 0303txt 0032
        Mesh indicator {
            3; 0;0;0;, 1;0;0;, 0;0;1;;
            1; 3;0,1,2;;
            MeshTextureCoords {3;0;0;,1;0;,0;1;;}
            MeshMaterialList {1;1;0;; Material {1;1;1;1;;0;0;0;0;;0;0;0;; TextureFilename {"base.png";} }}
        }
    "#);
    image::save_buffer(
        fixture.0.join("base.png"),
        &[255; 4],
        1,
        1,
        image::ColorType::Rgba8,
    )
    .unwrap();
    let mut tile = String::from("[version]\n14\n");
    for (id, name) in [(41, "state"), (42, "indicator"), (43, "helper")] {
        tile.push_str(&format!(
            "[object]\n0\n{name}.sco\n{id}\n100\n100\n0\n0\n0\n0\n2\n{name} label\n{id}\n\n",
        ));
    }
    fixture.write("tile_0_0.map", &tile);
    let world = Arc::new(World::open(&fixture.0, &fixture.0.join("global.cfg"), 20260101).unwrap());
    assert!(world.object_type("plain.sco").unwrap().program.is_none());
    for name in ["state.sco", "indicator.sco", "helper.sco"] {
        let ty = world.object_type(name).unwrap();
        assert!(ty.sco.scripts.scripts.is_empty());
        let program = ty
            .program
            .as_ref()
            .expect("declarations create instance state");
        assert!(program.errors.is_empty(), "{:?}", program.errors);
        assert!(program.var("enabled").is_some());
        assert!(program.str_var("label").is_some());
    }

    // The no-op backend executes the normal upload/placement path without a graphics
    // adapter. This checks live state and material selection, not rendered pixels.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::NOOP;
    descriptor.backend_options.noop = wgpu::NoopBackendOptions { enable: true };
    let instance = wgpu::Instance::new(descriptor);
    let renderer = pollster::block_on(Renderer::new_with(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        omsi_render::RenderOptions {
            msaa: 1,
            shadow_size: 1024,
            ..Default::default()
        },
    ))
    .unwrap();
    let mut scene = renderer.new_scene();
    let (prepared, mut stats) = world.prepare_tiles(&[(0, 0, fixture.0.join("tile_0_0.map"))]);
    assert_eq!(prepared.len(), 1);
    for tile in prepared {
        world.upload_tile(&renderer, &mut scene, tile, &mut stats);
    }
    assert_eq!(
        world.scripted.lock().len(),
        3,
        "retain visible and invisible variable-only objects"
    );
    let controllers = world.traffic_lights.lock();
    assert_eq!(controllers.len(), 1);
    assert_eq!(controllers[0].names, ["Outbound", "Return"]);
    drop(controllers);

    let mut app = crate::new_app(
        crate::Args::parse_from(["openomsi", "--root", fixture.0.to_str().unwrap()]),
        crate::settings::Settings::default(),
    );
    app.world = Some(world.clone());
    let snapshot = crate::plugin_api::execute(&mut app, "snapshot", json!({}), &[]).unwrap();
    let session = snapshot["session_id"].as_str().unwrap();
    let rows = crate::plugin_api::execute(&mut app, "scenery.list", json!({}), &[]).unwrap();
    let rows = rows["items"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let before = crate::plugin_api::execute(
            &mut app,
            "scenery.get",
            json!({"session_id":session,"id":id}),
            &[],
        )
        .unwrap();
        assert_eq!(before["strings"]["target"], row["map_id"]);
        assert_eq!(before["values"]["enabled"], 0.0);
        crate::plugin_api::execute(
            &mut app,
            "scenery.set",
            json!({
                "session_id":session,"id":id,"values":{"enabled":1},"strings":{"label":"updated"},
            }),
            &[],
        )
        .unwrap();
    }
    let variant = {
        let objects = world.scripted.lock();
        let indicator = objects.iter().find(|o| o.map_id == 42).unwrap();
        assert_eq!(indicator.variants.len(), 1);
        indicator.variants[0].clone()
    };
    assert_eq!(scene.instances[variant.0].materials[variant.1], variant.2);
    world.update_scripted(
        &renderer,
        &mut scene,
        0.016,
        DVec3::new(100.0, 100.0, 0.0),
        false,
        &|_, _| (0.0, 0.0),
        None,
        false,
    );
    assert_eq!(
        scene.instances[variant.0].materials[variant.1], variant.3,
        "plugin value drives the native material variant"
    );
    for row in rows {
        let after = crate::plugin_api::execute(
            &mut app,
            "scenery.get",
            json!({"session_id":session,"id":row["id"]}),
            &[],
        )
        .unwrap();
        assert_eq!(after["values"]["enabled"], 1.0);
        assert_eq!(after["strings"]["label"], "updated");
    }
    world.unload_tile(&renderer, &mut scene, (0, 0), None);
    for row in rows {
        assert!(crate::plugin_api::execute(
            &mut app,
            "scenery.get",
            json!({"session_id":session,"id":row["id"]}),
            &[]
        )
        .is_err());
    }
}
