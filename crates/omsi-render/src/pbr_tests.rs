use super::*;

fn image(rgba: [u8; 4]) -> omsi_texture::Image {
    omsi_texture::Image {
        width: 1,
        height: 1,
        rgba: rgba.to_vec(),
        has_alpha: true,
    }
}

#[test]
#[ignore = "requires a graphics adapter; renders authored road and terrain PBR maps"]
fn authored_pbr_shades_roads_and_terrain_without_changing_vanilla_or_coverage() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let mut renderer = pollster::block_on(Renderer::new_with(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        RenderOptions {
            msaa: 1,
            ssao: false,
            shadow_size: 1024,
            fxaa: false,
            render_scale: 1.0,
            ..Default::default()
        },
    ))
    .expect("test renderer");
    let camera = Camera {
        position: DVec3::new(0.0, -0.105, 6.0),
        yaw: 0.0,
        pitch: -89.0,
        roll: 0.0,
        fov_deg: 90.0,
        near: 0.1,
        far: 100.0,
    };
    let pixel = |rgba: &[u8], x: usize| -> [u8; 3] {
        rgba[(32 * 64 + x) * 4..(32 * 64 + x) * 4 + 3]
            .try_into()
            .unwrap()
    };
    for kind in ["road", "terrain", "painted"] {
        for map in ["normal", "height", "roughness"] {
            let mut scene = renderer.new_scene();
            let plain = renderer.add_texture(&mut scene, &image([100, 100, 100, 255]), false);
            let authored = renderer.add_texture(&mut scene, &image([100, 100, 100, 255]), false);
            let mask = renderer.add_texture(&mut scene, &image([255; 4]), false);
            let empty = renderer.add_texture(&mut scene, &image([0; 4]), false);
            renderer.add_pbr_maps(
                &mut scene,
                authored,
                &omsi_texture::pbr::PbrImages {
                    normal: (map != "roughness")
                        .then(|| image([64, 128, 238, if map == "height" { 128 } else { 255 }])),
                    height_scale: if map == "height" { 0.04 } else { 0.0 },
                    orm: (map == "roughness").then(|| image([255, 15, 0, 255])),
                    flags: if map != "roughness" {
                        [if map == "height" { 3.0 } else { 1.0 }, 0.0, 0.0, 0.0]
                    } else {
                        [0.0, 0.0, 1.0, 0.0]
                    },
                },
            );
            let material = |renderer: &Renderer, scene: &mut Scene, texture, mask| match kind {
                "terrain" => renderer.add_terrain_material(
                    scene,
                    Some(texture),
                    Some(mask),
                    None,
                    2.0,
                    None,
                    0.0,
                ),
                "painted" => renderer.add_terrain_layer_material(
                    scene,
                    Some(texture),
                    mask,
                    None,
                    2.0,
                    None,
                    0.0,
                ),
                _ => {
                    renderer.add_material(scene, Some(texture), AlphaMode::Opaque, [1.0; 4], false)
                }
            };
            let left = material(&renderer, &mut scene, authored, mask);
            let right = material(&renderer, &mut scene, plain, mask);
            let cut = material(&renderer, &mut scene, authored, empty);
            let mut quad = |left: f32, right: f32, material| {
                let mesh = renderer.add_mesh(
                    &mut scene,
                    &MeshData {
                        positions: vec![
                            Vec3::new(left, -5.0, 0.0),
                            Vec3::new(right, -5.0, 0.0),
                            Vec3::new(right, 5.0, 0.0),
                            Vec3::new(left, 5.0, 0.0),
                        ],
                        normals: vec![Vec3::Z; 4],
                        uvs: vec![
                            glam::Vec2::ZERO,
                            glam::Vec2::X,
                            glam::Vec2::ONE,
                            glam::Vec2::Y,
                        ],
                        indices: vec![0, 1, 2, 0, 2, 3],
                        ranges: vec![(0, 6, 0)],
                        one_sided: false,
                    },
                );
                let id = renderer.add_instance(
                    &mut scene,
                    mesh,
                    DVec3::ZERO,
                    Mat4::IDENTITY,
                    vec![material],
                );
                scene.instances[id].render_phase = if kind == "road" {
                    RenderPhase::Spline
                } else {
                    RenderPhase::Terrain
                };
                id
            };
            let left_instance = quad(-5.0, -0.5, left);
            quad(0.5, 5.0, right);
            let lighting = Lighting {
                sun_dir: if map != "roughness" {
                    Vec3::new(0.866, 0.0, 0.5)
                } else {
                    Vec3::new(-0.435, 0.0, 0.9).normalize()
                },
                sun_intensity: 1.0,
                shadows: false,
                detail: false,
                fog_density: 0.0,
                ..Default::default()
            };
            // Use the same mesh and pixel for the negative control. Separate left/right
            // pixels have different view vectors and can differ under GGX even without
            // a map, especially near the narrow highlight in the roughness case.
            scene.instances[left_instance].materials = vec![right];
            let vanilla_plain = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &lighting)
                .unwrap();
            let enhanced = Lighting {
                enhanced: true,
                ..lighting.clone()
            };
            let enhanced_plain = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &enhanced)
                .unwrap();
            scene.instances[left_instance].materials = vec![left];
            let vanilla_mapped = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &lighting)
                .unwrap();
            let (a, b) = (pixel(&vanilla_mapped, 16), pixel(&vanilla_plain, 16));
            assert!(
                (0..3).all(|i| a[i].abs_diff(b[i]) <= 3),
                "Vanilla {kind}/{map}: {a:?} != {b:?}"
            );
            let enhanced_mapped = renderer
                .render_to_image(&mut scene, 64, 64, &camera, &enhanced)
                .unwrap();
            let (a, b) = (pixel(&enhanced_mapped, 16), pixel(&enhanced_plain, 16));
            assert!(
                (0..3).any(|i| a[i].abs_diff(b[i]) >= 8),
                "Enhanced ignored {kind}/{map}: {a:?} vs {b:?}"
            );
            if kind != "road" {
                // An empty terrain brush/cut mask removes the authored material too.
                // Compare with the actual sky behind it, not a guessed background colour.
                scene.instances[left_instance].materials = vec![cut];
                let cut_picture = renderer
                    .render_to_image(&mut scene, 64, 64, &camera, &enhanced)
                    .unwrap();
                scene.instances[left_instance].visible = false;
                let absent_picture = renderer
                    .render_to_image(&mut scene, 64, 64, &camera, &enhanced)
                    .unwrap();
                let (a, b) = (pixel(&cut_picture, 16), pixel(&absent_picture, 16));
                assert!(
                    (0..3).all(|i| a[i].abs_diff(b[i]) <= 3),
                    "PBR changed {kind} coverage: {a:?} != {b:?}"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires a graphics adapter; compares parallax with an analytic recessed plane"]
fn height_parallax_matches_recessed_geometry_at_oblique_and_mirrored_views() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let mut renderer = pollster::block_on(Renderer::new_with(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        RenderOptions {
            msaa: 1,
            ssao: false,
            shadow_size: 1024,
            fxaa: false,
            render_scale: 1.0,
            ..Default::default()
        },
    ))
    .expect("test renderer");
    // Smooth repeated colour features avoid edge/mipmap tolerances. A constant
    // height field has an analytic intersection: a plane recessed by this depth.
    // Comparing real geometry tests the complete physical UV projection, alpha
    // upload and view-angle response without reproducing the shader's march.
    let size = 128usize;
    let mut rgba = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let wave = |v: usize| {
                (128.0 + 95.0 * (v as f32 / size as f32 * std::f32::consts::TAU * 4.0).sin())
                    .round() as u8
            };
            rgba.extend_from_slice(&[wave(x), wave(y), 80, 255]);
        }
    }
    let texture = omsi_texture::Image {
        width: size as u32,
        height: size as u32,
        rgba,
        has_alpha: false,
    };
    let range = 0.4f32;
    let depth = range * (1.0 - 128.0 / 255.0);
    let lighting = Lighting {
        enhanced: true,
        sun_dir: Vec3::new(0.3, -0.4, 0.866).normalize(),
        sun_intensity: 1.0,
        shadows: false,
        detail: false,
        fog_density: 0.0,
        ..Default::default()
    };
    for (yaw, pitch, position, mirror) in [
        (0.0, -89.0, DVec3::new(0.0, -0.0524, 3.0), 0),
        (0.0, -45.0, DVec3::new(0.0, -3.0, 3.0), 0),
        (90.0, -45.0, DVec3::new(-3.0, 0.0, 3.0), 0),
        (0.0, -45.0, DVec3::new(0.0, -3.0, 3.0), 2),
        (90.0, -45.0, DVec3::new(-3.0, 0.0, 3.0), 1),
    ] {
        let camera = Camera {
            position,
            yaw,
            pitch,
            roll: 0.0,
            fov_deg: 50.0,
            near: 0.1,
            far: 100.0,
        };
        let mut scene = renderer.new_scene();
        let diffuse = renderer.add_texture(&mut scene, &texture, true);
        let reference_diffuse = renderer.add_texture(&mut scene, &texture, true);
        renderer.add_pbr_maps(
            &mut scene,
            diffuse,
            &omsi_texture::pbr::PbrImages {
                normal: Some(image([128, 128, 255, 128])),
                height_scale: range,
                orm: None,
                flags: [3.0, 0.0, 0.0, 0.0],
            },
        );
        let mapped = renderer.add_material(
            &mut scene,
            Some(diffuse),
            AlphaMode::Opaque,
            [1.0; 4],
            false,
        );
        let plain = renderer.add_material(
            &mut scene,
            Some(reference_diffuse),
            AlphaMode::Opaque,
            [1.0; 4],
            false,
        );
        let data = |z| MeshData {
            positions: vec![
                Vec3::new(-4.0, -8.0, z),
                Vec3::new(4.0, -8.0, z),
                Vec3::new(4.0, 8.0, z),
                Vec3::new(-4.0, 8.0, z),
            ],
            normals: vec![Vec3::Z; 4],
            uvs: [
                glam::Vec2::ZERO,
                glam::Vec2::new(4.0, 0.0),
                glam::Vec2::splat(4.0),
                glam::Vec2::new(0.0, 4.0),
            ]
            .into_iter()
            .map(|uv| match mirror {
                1 => glam::Vec2::new(4.0 - uv.x, uv.y),
                2 => glam::Vec2::new(uv.x, 4.0 - uv.y),
                _ => uv,
            })
            .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
            ranges: vec![(0, 6, 0)],
            one_sided: false,
        };
        let surface = renderer.add_mesh(&mut scene, &data(0.0));
        let recessed = renderer.add_mesh(&mut scene, &data(-depth));
        let id = renderer.add_instance(
            &mut scene,
            surface,
            DVec3::ZERO,
            Mat4::IDENTITY,
            vec![mapped],
        );
        scene.instances[id].render_phase = RenderPhase::Spline;
        let actual = renderer
            .render_to_image(&mut scene, 96, 96, &camera, &lighting)
            .unwrap();
        scene.instances[id].materials = vec![plain];
        let negative = renderer
            .render_to_image(&mut scene, 96, 96, &camera, &lighting)
            .unwrap();
        scene.instances[id].mesh = recessed;
        let reference = renderer
            .render_to_image(&mut scene, 96, 96, &camera, &lighting)
            .unwrap();
        let mut error = 0u64;
        let mut effect = 0u64;
        let mut count = 0u64;
        for y in 36..60 {
            for x in 36..60 {
                for channel in 0..3 {
                    let at = (y * 96 + x) * 4 + channel;
                    error += actual[at].abs_diff(reference[at]) as u64;
                    effect += actual[at].abs_diff(negative[at]) as u64;
                    count += 1;
                }
            }
        }
        let error = error as f64 / count as f64;
        let effect = effect as f64 / count as f64;
        assert!(
            error <= 4.0,
            "yaw {yaw}, pitch {pitch}, mirror {mirror}: mean error {error} from recessed geometry"
        );
        if pitch > -80.0 {
            assert!(effect >= 8.0, "oblique parallax did not move colour: yaw {yaw}, mirror {mirror}, mean difference {effect}");
        }
        if pitch > -80.0 && mirror == 0 {
            let mut mask_rgba = Vec::new();
            for y in 0..32 {
                for x in 0..32 {
                    mask_rgba.extend_from_slice(&[
                        100,
                        100,
                        100,
                        if (x / 8 + y / 8) % 2 == 0 { 255 } else { 0 },
                    ]);
                }
            }
            let coverage = omsi_texture::Image {
                width: 32,
                height: 32,
                rgba: mask_rgba,
                has_alpha: true,
            };
            let mask = renderer.add_texture(&mut scene, &coverage, true);
            for kind in ["cut", "painted", "alpha"] {
                let mut materials = Vec::new();
                for height_scale in [0.0, range] {
                    let diffuse = renderer.add_texture(
                        &mut scene,
                        if kind == "alpha" {
                            &coverage
                        } else {
                            &image([100, 100, 100, 255])
                        },
                        true,
                    );
                    renderer.add_pbr_maps(
                        &mut scene,
                        diffuse,
                        &omsi_texture::pbr::PbrImages {
                            normal: Some(image([128, 128, 255, 128])),
                            height_scale,
                            orm: None,
                            flags: [3.0, 0.0, 0.0, 0.0],
                        },
                    );
                    materials.push(match kind {
                        "cut" => renderer.add_terrain_material(
                            &mut scene,
                            Some(diffuse),
                            Some(mask),
                            None,
                            1.0,
                            None,
                            0.0,
                        ),
                        "painted" => renderer.add_terrain_layer_material(
                            &mut scene,
                            Some(diffuse),
                            mask,
                            None,
                            1.0,
                            None,
                            0.0,
                        ),
                        _ => renderer.add_material(
                            &mut scene,
                            Some(diffuse),
                            AlphaMode::Test,
                            [1.0; 4],
                            false,
                        ),
                    });
                }
                scene.instances[id].mesh = surface;
                scene.instances[id].render_phase = if kind == "alpha" {
                    RenderPhase::Spline
                } else {
                    RenderPhase::Terrain
                };
                scene.instances[id].materials = vec![materials[0]];
                let original = renderer
                    .render_to_image(&mut scene, 96, 96, &camera, &lighting)
                    .unwrap();
                scene.instances[id].materials = vec![materials[1]];
                let parallax = renderer
                    .render_to_image(&mut scene, 96, 96, &camera, &lighting)
                    .unwrap();
                assert!(
                    original
                        .iter()
                        .zip(&parallax)
                        .all(|(a, b)| a.abs_diff(*b) <= 3),
                    "POM moved {kind} coverage at yaw {yaw}"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires a graphics adapter; checks physical height normals on rectangular repeats"]
fn height_normals_preserve_both_physical_axes_and_legacy_normal_conventions() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let mut renderer = pollster::block_on(Renderer::new_with(
        &instance,
        None,
        Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        RenderOptions {
            msaa: 1,
            ssao: false,
            shadow_size: 1024,
            fxaa: false,
            render_scale: 1.0,
            ..Default::default()
        },
    ))
    .expect("test renderer");
    let camera = Camera {
        position: DVec3::new(0.0, -0.035, 2.0),
        yaw: 0.0,
        pitch: -89.0,
        roll: 0.0,
        fov_deg: 65.0,
        near: 0.1,
        far: 100.0,
    };
    let lighting = Lighting {
        enhanced: true,
        sun_dir: Vec3::new(-0.5, -0.5, 0.7).normalize(),
        sun_intensity: 1.0,
        shadows: false,
        detail: false,
        fog_density: 0.0,
        ..Default::default()
    };
    for (flag, axis, mirror) in [
        (1.0, 0, 0),
        (1.0, 1, 0),
        (2.0, 0, 0),
        (2.0, 1, 0),
        (3.0, 0, 0),
        (3.0, 1, 0),
        (3.0, 2, 0),
        (3.0, 0, 1),
        (3.0, 1, 2),
    ] {
        let mut scene = renderer.new_scene();
        let plain = renderer.add_texture(&mut scene, &image([100, 100, 100, 255]), false);
        let authored = renderer.add_texture(&mut scene, &image([100, 100, 100, 255]), false);
        let rgba = if axis == 1 {
            [128, 192, 238, 255]
        } else {
            [192, 128, 238, 255]
        };
        renderer.add_pbr_maps(
            &mut scene,
            authored,
            &omsi_texture::pbr::PbrImages {
                normal: Some(image(rgba)),
                height_scale: 0.0,
                orm: None,
                flags: [flag, 0.0, 0.0, 0.0],
            },
        );
        let material = |scene: &mut Scene, texture| {
            renderer.add_terrain_material(scene, Some(texture), None, None, 1.0, None, 0.0)
        };
        let plain_material = material(&mut scene, plain);
        let authored_material = material(&mut scene, authored);
        // One repeat covers 2 x 4 metres. Compare the mapped result with the same
        // surface carrying the analytic world normal, through the complete shader.
        let mut expected =
            Vec3::new(rgba[0] as f32, rgba[1] as f32, rgba[2] as f32) / 255.0 * 2.0 - Vec3::ONE;
        if flag < 2.5 {
            // Preserve the existing DX/GL convention. With this downward camera the
            // legacy cofactors point along -X/-Y, and the shared scale halves V.
            expected.x = -expected.x;
            expected.y *= if flag > 1.5 { 0.5 } else { -0.5 };
        } else {
            // Physical slopes follow increasing U/V in the surface, independent of
            // screen orientation. Mirroring one UV axis reverses only that direction.
            if mirror == 1 {
                expected.x = -expected.x;
            }
            if mirror == 2 {
                expected.y = -expected.y;
            }
        }
        expected = if axis == 2 {
            Vec3::Z
        } else {
            expected.normalize()
        };
        let data = |normal| MeshData {
            positions: vec![
                Vec3::new(-1.0, -2.0, 0.0),
                Vec3::new(1.0, -2.0, 0.0),
                Vec3::new(1.0, 2.0, 0.0),
                Vec3::new(-1.0, 2.0, 0.0),
            ],
            normals: vec![normal; 4],
            uvs: if axis == 2 {
                // One collapsed UV axis cannot define a physical tangent frame.
                vec![
                    glam::Vec2::ZERO,
                    glam::Vec2::X,
                    glam::Vec2::X,
                    glam::Vec2::ZERO,
                ]
            } else {
                [
                    glam::Vec2::ZERO,
                    glam::Vec2::X,
                    glam::Vec2::ONE,
                    glam::Vec2::Y,
                ]
                .into_iter()
                .map(|uv| match mirror {
                    1 => glam::Vec2::new(1.0 - uv.x, uv.y),
                    2 => glam::Vec2::new(uv.x, 1.0 - uv.y),
                    _ => uv,
                })
                .collect()
            },
            indices: vec![0, 1, 2, 0, 2, 3],
            ranges: vec![(0, 6, 0)],
            one_sided: false,
        };
        let authored_mesh = renderer.add_mesh(&mut scene, &data(Vec3::Z));
        let reference_mesh = renderer.add_mesh(&mut scene, &data(expected));
        let id = renderer.add_instance(
            &mut scene,
            authored_mesh,
            DVec3::ZERO,
            Mat4::IDENTITY,
            vec![authored_material],
        );
        scene.instances[id].render_phase = RenderPhase::Terrain;
        let actual = renderer
            .render_to_image(&mut scene, 64, 64, &camera, &lighting)
            .unwrap();
        scene.instances[id].mesh = reference_mesh;
        scene.instances[id].materials = vec![plain_material];
        let reference = renderer
            .render_to_image(&mut scene, 64, 64, &camera, &lighting)
            .unwrap();
        let offset = (32 * 64 + 32) * 4;
        let (a, b) = (&actual[offset..offset + 3], &reference[offset..offset + 3]);
        assert!(
            (0..3).all(|i| a[i].abs_diff(b[i]) <= 3),
            "flag {flag}, axis {axis}, mirror {mirror}: {a:?} != analytic {b:?}"
        );
    }
}
