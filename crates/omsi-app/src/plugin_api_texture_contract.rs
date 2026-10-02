//! Opt-in GPU contract. Uses isolated authored content, real vehicle upload and
//! production API dispatch; never opens a window or the user's bridge manifest.
use clap::Parser;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "openomsi-texture-contract-{}-{}",
            std::process::id(),
            crate::plugin_api::random_id()
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

// Read the actual texture through a GPU compute pass. Scene texture resources are
// intentionally not COPY_SRC; sampling them avoids changing production usages or
// adding a public renderer diagnostic method solely for this test. sRGB formats
// produce linear RGB, while alpha remains linear.
fn read_gpu(
    renderer: &omsi_render::Renderer,
    scene: &omsi_render::Scene,
    id: omsi_render::TextureId,
) -> Vec<[f32; 4]> {
    let (width, height, _) = renderer.texture_levels(scene, id).unwrap();
    let view = renderer.texture_view(scene, id).unwrap();
    let device = &renderer.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("API texture contract readback"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
            @group(0) @binding(0) var image: texture_2d<f32>;
            @group(0) @binding(1) var<storage,read_write> pixels: array<vec4<f32>>;
            @compute @workgroup_size(8,8)
            fn main(@builtin(global_invocation_id) id:vec3<u32>) {
                let size=textureDimensions(image);
                if id.x<size.x && id.y<size.y {
                    pixels[id.y*size.x+id.x]=textureLoad(image,vec2<i32>(id.xy),0);
                }
            }
        "#
            .into(),
        ),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("API texture contract"),
        layout: None,
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bytes = u64::from(width) * u64::from(height) * 16;
    let storage = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: storage.as_entire_binding(),
            },
        ],
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
    }
    encoder.copy_buffer_to_buffer(&storage, 0, &readback, 0, bytes);
    let submission = renderer.queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (send, recv) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = send.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: None,
        })
        .unwrap();
    recv.recv().unwrap().unwrap();
    let mapped = slice.get_mapped_range();
    let result = mapped
        .chunks_exact(16)
        .map(|pixel| {
            std::array::from_fn(|c| f32::from_le_bytes(pixel[c * 4..c * 4 + 4].try_into().unwrap()))
        })
        .collect();
    drop(mapped);
    readback.unmap();
    result
}
fn assert_rgba(app: &crate::App, id: omsi_render::TextureId, expected: &[u8]) {
    let pixels = read_gpu(
        app.renderer.as_ref().unwrap(),
        app.scene.as_ref().unwrap(),
        id,
    );
    assert_eq!(pixels.len() * 4, expected.len());
    for (index, (actual, expected)) in pixels.iter().zip(expected.chunks_exact(4)).enumerate() {
        for channel in 0..4 {
            let value = expected[channel] as f32 / 255.0;
            let linear = if channel == 3 || value <= 0.04045 {
                if channel == 3 {
                    value
                } else {
                    value / 12.92
                }
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            };
            assert!(
                (actual[channel] - linear).abs() < 0.0005,
                "pixel {index}, channel {channel}: {} != {linear}",
                actual[channel]
            );
        }
    }
}
fn upload(
    app: &mut crate::App,
    identity: &Value,
    section: usize,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Value {
    let mut args = identity.clone();
    args.as_object_mut().unwrap().extend(
        json!({"section":section,"index":2,"width":width,"height":height,"format":"bgra8"})
            .as_object()
            .unwrap()
            .clone(),
    );
    crate::plugin_api::execute(app, "texture.upload", args, bgra).unwrap()
}

#[test]
#[ignore = "requires a working graphics adapter; creates an offscreen renderer"]
fn native_texture_pixels_leases_resize_and_placed_vehicle_cleanup() {
    let fixture = Fixture::new();
    fixture.write("global.cfg", "[name]\nTexture API fixture\n");
    fixture.write("bus.bus", "[model]\nmodel.cfg\n[mass]\n10000\n");
    fixture.write(
        "trailer.bus",
        "[model]\nmodel.cfg\n[scriptshare]\n[mass]\n10000\n",
    );
    fixture.write("model.cfg","[scripttexture]\n2\n2\n[scripttexture]\n2\n2\n[scripttexture]\n2\n2\n[mesh]\nscreen.x\n[matl]\nscreen.png\n0\n[useScriptTexture]\n2\n");
    fixture.write("screen.x",r#"xof 0303txt 0032
        Mesh screen {
            3; -1;0;0;, 1;0;0;, 0;0;1;;
            1; 3;0,1,2;;
            MeshTextureCoords {3;0;0;,1;0;,0.5;1;;}
            MeshMaterialList {1;1;0;; Material {1;1;1;1;;0;0;0;0;;0;0;0;; TextureFilename {"screen.png";} }}
        }
    "#);
    image::save_buffer(
        fixture.0.join("screen.png"),
        &[255, 255, 255, 255],
        1,
        1,
        image::ColorType::Rgba8,
    )
    .unwrap();
    let instance = crate::graphics_instance();
    let renderer = pollster::block_on(omsi_render::Renderer::new(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
    ))
    .unwrap();
    let mut scene = renderer.new_scene();
    let ty = Arc::new(omsi_sim::VehicleType::load(&fixture.0, &fixture.0.join("bus.bus")).unwrap());
    assert_eq!(ty.model.script_textures.len(), 3);
    assert_eq!(
        ty.meshes.len(),
        1,
        "the authored screen must load, not just its texture descriptor"
    );
    let world = Arc::new(
        crate::scene::World::open(&fixture.0, &fixture.0.join("global.cfg"), 20261001).unwrap(),
    );
    let render = world.add_vehicle(&renderer, &mut scene, &ty, None);
    let trailer = omsi_sim::VehicleType::load(&fixture.0, &fixture.0.join("trailer.bus")).unwrap();
    let shared = world.add_vehicle_part(&renderer, &mut scene, &trailer, None, &render);
    let texture = render.script_textures[2].unwrap();
    assert!(
        render
            .own_materials
            .iter()
            .any(|m| scene.materials[*m].uses_texture(texture)),
        "the authored material must reference its live script slot"
    );
    assert!(shared.shared_script);
    assert_eq!(shared.script_textures[2], Some(texture));
    assert!(
        shared
            .own_materials
            .iter()
            .any(|m| scene.materials[*m].uses_texture(texture)),
        "the authored trailer material must share the leading script slot"
    );
    let mut player = crate::plugin_api_contract::player(omsi_sim::VehicleInstance::new(
        ty,
        omsi_sim::VehicleHost::default(),
    ));
    player.render = render;
    // The real script-sharing trailer render is enough for texture ownership;
    // this test does not run articulated vehicle physics.
    player.trailer_renders.push(shared);
    let original = [255, 0, 0, 255].repeat(4);
    player.vehicle.host.script_textures[2].rgba = original.clone();
    player.vehicle.host.script_textures[2].dirty = true;
    crate::scene::sync_vehicle_textures(
        &renderer,
        &mut scene,
        &mut player.vehicle,
        &player.render,
        &mut { usize::MAX },
    );
    let mut app = crate::new_app(
        crate::Args::parse_from(["openomsi", "--root", fixture.0.to_str().unwrap()]),
        crate::settings::Settings::default(),
    );
    app.renderer = Some(renderer);
    app.scene = Some(scene);
    app.world = Some(world);
    app.player = Some(player);
    let snapshot = crate::plugin_api::execute(&mut app, "snapshot", json!({}), &[]).unwrap();
    let identity = json!({"id":snapshot["vehicle"]["id"],"generation":snapshot["vehicle"]["generation"],"session_id":snapshot["session_id"]});
    app.native_bridge = Some(
        crate::native_bridge::Bridge::start_for_test(&fixture.0, &fixture.0.join("bridge.json"))
            .unwrap(),
    );
    crate::native_bridge::poll(&mut app);
    let first_connection = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let second_connection = Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert_rgba(&app, texture, &original);
    let scene_textures = app.scene.as_ref().unwrap().textures.len();
    let bgra = [
        255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 64, 255, 255, 255, 0,
    ]
    .repeat(2);
    let rgba = [
        0, 0, 255, 128, 0, 255, 0, 255, 255, 0, 0, 64, 255, 255, 255, 0,
    ]
    .repeat(2);
    let first = upload(&mut app, &identity, 0, 4, 2, &bgra);
    app.native_bridge
        .as_mut()
        .unwrap()
        .own_texture_for_test(first_connection.clone(), first.clone());
    assert_eq!(
        app.renderer
            .as_ref()
            .unwrap()
            .texture_levels(app.scene.as_ref().unwrap(), texture),
        Some((4, 2, 1))
    );
    assert_rgba(&app, texture, &rgba);
    assert_eq!(
        app.scene.as_ref().unwrap().textures.len(),
        scene_textures,
        "resize must retain the GPU resource slot"
    );
    let mut stale = identity.clone();
    stale["generation"] = json!("0");
    stale.as_object_mut().unwrap().extend(
        json!({"index":2,"width":1,"height":1,"format":"bgra8"})
            .as_object()
            .unwrap()
            .clone(),
    );
    assert!(crate::plugin_api::execute(&mut app, "texture.upload", stale, &[255; 4]).is_err());
    assert_rgba(&app, texture, &rgba);
    {
        let player = app.player.as_mut().unwrap();
        assert!(player.render.external_script_textures.contains(&2));
        assert!(player.trailer_renders[0]
            .external_script_textures
            .contains(&2));
        player.vehicle.host.script_textures[2].rgba = [0, 255, 0, 255].repeat(4);
        player.vehicle.host.script_textures[2].dirty = true;
        crate::scene::sync_vehicle_textures(
            app.renderer.as_ref().unwrap(),
            app.scene.as_mut().unwrap(),
            &mut player.vehicle,
            &player.render,
            &mut { usize::MAX },
        );
    }
    assert_rgba(&app, texture, &rgba); // Script redraw cannot overwrite an external lease.
    let latest = upload(&mut app, &identity, 1, 1, 1, &[0, 0, 255, 255]);
    app.native_bridge
        .as_mut()
        .unwrap()
        .own_texture_for_test(second_connection.clone(), latest.clone());
    assert_rgba(&app, texture, &[255, 0, 0, 255]);
    assert!(
        crate::plugin_api::execute(&mut app, "texture.release", first, &[]).is_err(),
        "shared trailer upload supersedes leading lease"
    );
    first_connection.store(false, std::sync::atomic::Ordering::Release);
    crate::native_bridge::poll(&mut app);
    assert_rgba(&app, texture, &[255, 0, 0, 255]); // Old connection cannot clear a newer owner's pixels.
                                                   // A bus that stopped being the player still needs its own texture restored.
    let player = app.player.take().unwrap();
    app.placed.push(player);
    // A reconnect/other client must not postpone cleanup of this disconnected owner.
    second_connection.store(false, std::sync::atomic::Ordering::Release);
    crate::native_bridge::poll(&mut app);
    assert_eq!(
        app.renderer
            .as_ref()
            .unwrap()
            .texture_levels(app.scene.as_ref().unwrap(), texture),
        Some((2, 2, 1))
    );
    assert_rgba(&app, texture, &[0, 255, 0, 255].repeat(4));
    assert!(app.placed[0].render.external_script_textures.is_empty());
    assert!(app.placed[0].trailer_renders[0]
        .external_script_textures
        .is_empty());
    assert!(
        crate::plugin_api::execute(&mut app, "texture.release", latest.clone(), &[]).is_err(),
        "lease cannot restore twice"
    );
    assert_eq!(
        app.native_bridge
            .as_ref()
            .unwrap()
            .owned_texture_count_for_test(),
        0
    );
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let explicit = upload(&mut app, &identity, 0, 1, 1, &[0, 0, 255, 255]);
    app.native_bridge
        .as_mut()
        .unwrap()
        .own_texture_for_test(connected, explicit.clone());
    crate::plugin_api::execute(&mut app, "texture.release", explicit, &[]).unwrap();
    crate::native_bridge::poll(&mut app);
    assert_eq!(
        app.native_bridge
            .as_ref()
            .unwrap()
            .owned_texture_count_for_test(),
        0,
        "successful release must retire metadata while its connection stays alive"
    );
    app.placed.clear();
    assert!(
        crate::plugin_api::execute(&mut app, "texture.release", latest, &[]).is_err(),
        "unloaded vehicle must reject old receipts"
    );
    println!("GPU texture contract: authored screen material, complete BGRA/alpha readback, resize, native redraw exclusion, shared trailer lease, per-connection disconnect and placed/unloaded vehicle cleanup passed.");
}
