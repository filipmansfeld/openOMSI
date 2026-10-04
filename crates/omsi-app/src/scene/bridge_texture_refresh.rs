//! Real scene resources for files produced after the vehicle was uploaded.
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("openomsi-declared-refresh-{}-{}",
            std::process::id(), std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(path.join("Vehicles/Own/Texture")).unwrap();
        std::fs::create_dir_all(path.join("Vehicles/Sibling/Texture")).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, data: &str) {
        std::fs::write(self.0.join(path), data).unwrap();
    }
    fn png(&self, path: &str, width: u32) {
        image::save_buffer(self.0.join(path), &vec![255; width as usize * 4],
            width, 1, image::ColorType::Rgba8).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Ok(path), Ok(root)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
            if path.starts_with(&root) && path != root { let _ = std::fs::remove_dir_all(path); }
        }
    }
}

#[test]
fn declared_file_refresh_attaches_missing_own_and_sibling_materials_and_enforces_budget() {
    let fixture = Fixture::new();
    fixture.write("global.cfg", "[name]\nDeclared texture refresh\n");
    fixture.write("Vehicles/Own/test.bus", "[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n");
    fixture.write("Vehicles/Own/vars.txt", "selector\n");
    fixture.write("Vehicles/Own/model.cfg", "[LOD]\n1000\n[mesh]\ntriangle.x\n[texchanges]\nentries.cfg\n");
    fixture.write("Vehicles/Own/triangle.x", r#"xof 0303txt 0032
        Mesh display {
            3; 0;0;0;, 1;0;0;, 0;0;1;;
            1; 3;0,1,2;;
            MeshTextureCoords {3;0;0;,1;0;,0;1;;}
            MeshMaterialList {1;1;0;; Material {1;1;1;1;;0;0;0;0;;0;0;0;; TextureFilename {"display.png";} }}
        }
    "#);
    let mut entries: Vec<String> = (0..65).map(|i| format!("Texture\\{i}.png")).collect();
    entries[1] = "..\\Sibling\\Texture\\1.png".into();
    fixture.write("Vehicles/Own/entries.cfg", &format!(
        "[newtexchangemaster]\ndisplay.png\nselector\n[entries]\n65\n{}\n", entries.join("\n")));
    let world = World::open(&fixture.0, &fixture.0.join("global.cfg"), 20260101).unwrap();
    let ty = Arc::new(omsi_sim::VehicleType::load(&fixture.0,
        &fixture.0.join("Vehicles/Own/test.bus")).unwrap());
    assert!(ty.program.errors.is_empty(), "{:?}", ty.program.errors);
    assert_eq!(ty.texchanges.len(), 1);
    let mut vehicle = omsi_sim::VehicleInstance::new(ty.clone(), omsi_sim::host::VehicleHost::default());
    assert!(vehicle.set_var("selector", 0.0));

    // The normal renderer upload and material selection run on wgpu's no-op backend.
    // This verifies live GPU resources/bindings; it does not claim pixel evidence.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::NOOP;
    descriptor.backend_options.noop = wgpu::NoopBackendOptions { enable: true };
    let instance = wgpu::Instance::new(descriptor);
    let renderer = pollster::block_on(Renderer::new_with(&instance, None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb), omsi_render::RenderOptions {
            msaa: 1, shadow_size: 1024, ..Default::default()
        })).unwrap();
    let mut scene = renderer.new_scene();
    let mut render = world.add_vehicle(&renderer, &mut scene, &ty, None);
    assert_eq!(render.variants.len(), 1);
    assert_eq!(render.variants[0].entries.len(), 65);
    assert!(render.variants[0].entry_tex.iter().all(Option::is_none));
    let slot = &render.variants[0];
    assert!(scene.materials[scene.instances[render.instances[slot.mesh]].materials[slot.slot]].texture.is_none());

    // Both files are created after upload, including the declared sibling path.
    fixture.png("Vehicles/Own/Texture/0.png", 1);
    fixture.png("Vehicles/Sibling/Texture/1.png", 1);
    for index in [0, 1] {
        world.bridge_refresh_texture_sections(&renderer, &mut scene,
            &mut [(&ty, &mut render)], &entries[index]).unwrap();
        assert!(vehicle.set_var("selector", index as f32));
        sync_vehicle_materials(&renderer, &mut scene, &vehicle, &mut render);
        let slot = &render.variants[0];
        let texture = slot.entry_tex[index].expect("fresh PNG has a live texture");
        let selected = scene.instances[render.instances[slot.mesh]].materials[slot.slot];
        assert_eq!(selected, slot.entries[index].0);
        assert_eq!(scene.materials[selected].texture, Some(texture), "live mesh selects the newly loaded entry");
        assert_eq!(renderer.texture_levels(&scene, texture), Some((1, 1, 1)));
    }
    let previous = render.variants[0].entry_tex[1].unwrap();
    let material_count = render.own_materials.len();
    fixture.png("Vehicles/Sibling/Texture/1.png", 2);
    world.bridge_refresh_texture_sections(&renderer, &mut scene,
        &mut [(&ty, &mut render)], &entries[1]).unwrap();
    sync_vehicle_materials(&renderer, &mut scene, &vehicle, &mut render);
    assert_eq!(render.variants[0].entry_tex[1], Some(previous));
    assert_eq!(renderer.texture_levels(&scene, previous).map(|(w,h,_)| (w,h)), Some((2, 1)));
    assert_eq!(render.own_materials.len(), material_count, "refresh reuses the material and counted resource");
    assert_eq!(world.vehicle_textures.lock().len(), 2);

    fixture.png("Vehicles/Sibling/Texture/undeclared.png", 1);
    for rejected in ["..\\Sibling\\Texture\\undeclared.png", "/absolute.png", "C:\\absolute.png"] {
        assert!(world.bridge_refresh_texture_sections(&renderer, &mut scene,
            &mut [(&ty, &mut render)], rejected).is_err(), "{rejected}");
    }
    for index in 2..64 {
        fixture.png(&format!("Vehicles/Own/Texture/{index}.png"), 1);
        world.bridge_refresh_texture_sections(&renderer, &mut scene,
            &mut [(&ty, &mut render)], &entries[index]).unwrap();
    }
    assert_eq!(world.bridge_texture_paths.lock().len(), 64);
    fixture.png("Vehicles/Own/Texture/64.png", 1);
    let materials = render.own_materials.len();
    let textures = scene.textures.len();
    assert_eq!(world.bridge_refresh_texture_sections(&renderer, &mut scene,
        &mut [(&ty, &mut render)], &entries[64]).unwrap_err(), "external file texture limit reached");
    assert_eq!(render.variants[0].entry_tex[64], None);
    assert_eq!(render.own_materials.len(), materials);
    assert_eq!(scene.textures.len(), textures, "budget refusal happens before GPU allocation/rebinding");
    world.release_vehicle(&renderer, &mut scene, render);
    assert!(world.vehicle_textures.lock().is_empty(), "counted refresh textures leave with their vehicle");
}
