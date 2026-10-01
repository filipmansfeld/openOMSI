//! Integration contract: Lua calls the production PluginIo and engine dispatcher.
use crate::*;
use std::path::PathBuf;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "openomsi-engine-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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

pub(crate) fn player(vehicle: omsi_sim::VehicleInstance) -> Player {
    Player {
        uid: 7,
        vehicle,
        render: scene::VehicleRender {
            instances: vec![],
            own_materials: vec![],
            set: None,
            variants: vec![],
            text_textures: vec![],
            script_textures: vec![],
            external_script_textures: Default::default(),
            shared_script: false,
            displays_far: false,
            display_tick: 0,
            skinned: vec![],
            hidden: false,
            interior_lamps: Default::default(),
            interior_blocks: Default::default(),
        },
        trailer_renders: vec![],
        axes: Default::default(),
        analog: Default::default(),
        cam_choice: (0, 0),
        bindings: vec![],
        sounds: None,
        pressed_mesh: None,
        pressed_trailer_mesh: None,
        press_info: (true, 0.0),
        auto_drag: None,
        startup: None,
        startup_at: None,
        give_ticket: false,
        give_change: false,
        hand_coupled: 0,
        rail_bound: false,
        rail: None,
        cam_before_special: None,
        held_keys: Default::default(),
        head: Vec3::ZERO,
        head_vel: Vec3::ZERO,
        head_omega: Vec3::ZERO,
        seat: Vec3::ZERO,
        mirror_offsets: vec![],
        mirrors_dirty: false,
        take_change: false,
        toggled_up: Default::default(),
        side_lights_by_l: false,
        driver: None,
        ibis_duty: None,
        ibis_typist: None,
        duty_typed: false,
        html_next_stop: None,
        ibis_background: false,
        arm: Default::default(),
        blinker_key_state: 0,
        steer_look: 0.0,
    }
}

#[test]
fn lua_plugin_mutates_real_engine_state_and_rejects_stale_writes() {
    let fixture = Fixture::new();
    fixture.write("bus.bus","[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n[stringvarnamelist]\n1\nstrings.txt\n[script]\n1\nscript.osc\n[constfile]\n1\nconstants.txt\n[mass]\n10000\n");
    fixture.write("model.cfg", "");
    fixture.write(
        "vars.txt",
        "test_value\nobserved_brake\nBrake\nThrottle\nClutch\n",
    );
    fixture.write("strings.txt", "test_text\n");
    fixture.write("script.osc","{trigger:test_increment}\n(L.L.test_value) (C.L.increment) + (S.L.test_value)\n{end}\n{frame}\n(L.L.Brake) (S.L.observed_brake)\n{end}\n");
    fixture.write(
        "constants.txt",
        "[const]\nincrement\n1\n[newcurve]\ntest_curve\n[pnt]\n0\n10\n[pnt]\n1\n20\n",
    );
    let ty = std::sync::Arc::new(
        omsi_sim::VehicleType::load(&fixture.0, &fixture.0.join("bus.bus")).unwrap(),
    );
    let mut vehicle = omsi_sim::VehicleInstance::new(
        ty.clone(),
        omsi_sim::VehicleHost::new(omsi_sim::SimClock::default()),
    );
    vehicle.enable_rigid_body();
    let mut app = new_app(
        Args::parse_from(["openomsi", "--root", fixture.0.to_str().unwrap()]),
        settings::Settings::default(),
    );
    app.player = Some(player(vehicle));
    app.camera = Some(omsi_render::Camera {
        position: DVec3::ZERO,
        yaw: 0.0,
        pitch: 0.0,
        roll: 0.0,
        fov_deg: 60.0,
        near: 0.2,
        far: 6000.0,
    });
    app.weather = Some(omsi_content::weather::Weather {
        temp: (20.0, 8.0),
        ..Default::default()
    });
    fixture.write("contract.lua",r#"
        local saved
        function on_frame()
            if saved then
                local ok = pcall(omsi.api,"vehicle.set_variables",saved)
                assert(not ok, "stale write must fail")
                omsi.message("native-stale-ok",1)
                return
            end
            local v=omsi.api("vehicle.get")
            local session=omsi.api("snapshot").session_id
            assert(type(v.id)=="string" and type(v.generation)=="string")
            local args={id=v.id,generation=v.generation,session_id=session,
                values={test_value=41},strings={test_text="Příští zastávka"}}
            omsi.api("vehicle.set_variables",args)
            omsi.api("vehicle.trigger",{name="test_increment"})
            assert(omsi.var("test_value")==42)
            local constants=omsi.api("vehicle.script",{kind="constants"}).items
            assert(constants[1].name=="increment" and constants[1].value==1)
            local curves=omsi.api("vehicle.script",{kind="curves"}).items
            assert(curves[1].name=="test_curve" and curves[1].points[2][2]==20)
            assert(omsi.api("vehicle.curve",{name="TEST_CURVE",x=0.5}).value==15)
            assert(omsi.api("vehicle.curve",{name="test_curve",x=-1}).value==10)
            assert(not pcall(omsi.api,"vehicle.curve",{name="missing",x=1}))
            assert(not pcall(omsi.api,"vehicle.script",{kind=3}))
            omsi.api("vehicle.set_controls",{controls={brake=0.75}})
            assert(omsi.api("vehicle.physics").pending_controls.brake==0.75)
            local ok=pcall(omsi.api,"vehicle.set_variables",{values={test_value=999,not_declared=1}})
            assert(not ok and omsi.var("test_value")==42)
            omsi.api("vehicle.set_pose",{position={east=-392.2,north=12,up=5},heading_degrees=90,preserve_velocity=false})
            omsi.api("vehicle.set_velocity",{velocity_world={3,4,5},angular_velocity_body={0,0,0.1}})
            assert(omsi.api("vehicle.physics").rigid.velocity_world[1]==3)
            local coefficients=omsi.api("vehicle.physics.parameters")
            assert(coefficients.mass_kg==10000)
            omsi.api("vehicle.physics.configure",{values={mass_kg=12000,center_of_gravity_body_metres={0,0,2}}})
            assert(omsi.api("vehicle.physics.parameters").mass_kg==12000)
            omsi.api("vehicle.head.set",{position_body_metres={0.2,0.1,0.05},velocity_body_metres_per_second={0,0,0}})
            assert(math.abs(omsi.api("vehicle.head").position_body_metres[1]-0.2)<0.0001)
            assert(math.abs(omsi.api("vehicle.get").head_position.y-0.05)<0.0001)
            assert(not pcall(omsi.api,"vehicle.head.set",{position_body_metres={1,1,1},velocity_body_metres_per_second={0,1000,0}}))
            assert(math.abs(omsi.api("vehicle.head").position_body_metres[1]-0.2)<0.0001)
            assert(omsi.api("vehicle.physics").rigid.velocity_world[1]==3)
            assert(not pcall(omsi.api,"vehicle.physics.configure",{values={mass_kg=15000,inertia_body_kg_m2={1,0,3}}}))
            assert(omsi.api("vehicle.physics.parameters").mass_kg==12000)
            assert(not pcall(omsi.api,"vehicle.set_velocity",{velocity_world={1e30,0,0}}))
            assert(not pcall(omsi.api,"vehicle.set_speed",{metres_per_second=1e30}))
            assert(omsi.api("vehicle.physics").rigid.velocity_world[1]==3)
            omsi.api("weather.set",{values={temperature_celsius=-3,road_wetness=0.5}})
            assert(omsi.api("weather.get").temperature_celsius==-3)
            assert(not pcall(omsi.api,"weather.set",{values={temperature_celsius=7,road_wetness=9}}))
            assert(omsi.api("weather.get").temperature_celsius==-3)
            omsi.api("clock.set",{service_seconds=12345.25})
            assert(omsi.api("clock.get").service_seconds==12345.25)
            omsi.api("camera.select",{mode="free"})
            omsi.api("camera.set",{position={east=123,north=234,up=5},heading_degrees=90,pitch_degrees=-20,fov_degrees=80})
            local cam=omsi.api("camera.get")
            assert(cam.position.east==123 and cam.heading_degrees==90 and math.abs(cam.fov_degrees-80)<0.0001)
            assert(not pcall(omsi.api,"camera.set",{position={east=999,north=234,up=5},near_metres=9000}))
            assert(omsi.api("camera.get").position.east==123)
            omsi.api("camera.select",{mode="outside"})
            omsi.api("camera.set",{look_yaw_degrees=25,look_pitch_degrees=10,orbit_metres=12,fov_degrees=75})
            cam=omsi.api("camera.get")
            assert(cam.look_yaw_degrees==25 and cam.orbit_metres==12)
            assert(not pcall(omsi.api,"camera.set",{position={east=0,north=0,up=0}}))
            assert(not pcall(omsi.api,"camera.select",{mode="driver",index=999}))
            assert(omsi.api("camera.get").mode=="outside")
            saved=args
            omsi.message("native-engine-ok",1)
        end
    "#);
    let mut plugins =
        omsi_plugin::Plugins::load(&[fixture.0.clone()], &omsi_plugin::HostConfig::default());
    assert_eq!(plugins.lua.len(), 1);
    let frame = |app: &mut App, plugins: &mut omsi_plugin::Plugins| {
        let info = plugins::game_info(app);
        let mut io = plugins::Io {
            app,
            dt: 0.016,
            message: None,
            info,
            commands: vec![],
            keys: vec![],
        };
        plugins.frame(&mut io);
        io.message.map(|(s, _)| s)
    };
    assert_eq!(
        frame(&mut app, &mut plugins).as_deref(),
        Some("native-engine-ok")
    );
    let v = &app.player.as_ref().unwrap().vehicle;
    assert_eq!(v.var("test_value"), Some(42.0));
    assert_eq!(v.str_var("test_text"), "Příští zastávka");
    assert_eq!(v.position, DVec3::new(-392.2, 12.0, 5.0));
    assert_eq!(v.host.temperature, -3.0);
    let p = app.player.as_mut().unwrap();
    let head_before = p.head.x;
    p.move_head(0.016, true, false);
    assert!(
        p.head.x < head_before && p.head_vel.x < 0.0,
        "the actual head spring must continue from the API state"
    );
    // Normal keyboard/controller sampling must not erase the plugin command before
    // the next real script/physics frame observes it; a subsequent step is native again.
    let v = &mut app.player.as_mut().unwrap().vehicle;
    v.set_controls(Default::default());
    v.update(0.016);
    assert_eq!(v.var("observed_brake"), Some(0.75));
    assert!(v.queued_native_controls().is_none());
    v.update(0.016);
    assert_eq!(v.var("observed_brake"), Some(0.0));
    v.rigid = None;
    plugin_api_vehicle::execute(
        v,
        "vehicle.set_pose",
        &serde_json::json!({"pitch_degrees":-5,"bank_degrees":-7}),
    )
    .unwrap()
    .unwrap();
    v.update(0.016);
    assert!(v.pitch <= 0.0 && v.pitch >= -5.0, "{}", v.pitch);
    assert!(v.bank <= 0.0 && v.bank >= -7.0, "{}", v.bank);
    v.ground = Some(std::sync::Arc::new(|x, _| Some(x)));
    v.update_ground_probe();
    plugin_api_vehicle::execute(v,"vehicle.set_pose",&serde_json::json!({"position":{"east":10,"north":0,"up":5},"heading_degrees":0,"pitch_degrees":0,"bank_degrees":0})).unwrap().unwrap();
    assert_eq!(v.host.ground_probe.as_ref().unwrap()(0.0, 0.0, 0.0), -5.0);
    // Simulate unloading then reloading the same type at the same public UID. Refresh
    // while absent ensures that retained identities cannot select the replacement.
    app.player = None;
    plugin_api::execute(&mut app, "snapshot", serde_json::json!({}), &[]).unwrap();
    app.player = Some(player(omsi_sim::VehicleInstance::new(
        ty,
        omsi_sim::VehicleHost::default(),
    )));
    assert_eq!(
        frame(&mut app, &mut plugins).as_deref(),
        Some("native-stale-ok")
    );
    assert_eq!(
        app.player.as_ref().unwrap().vehicle.var("test_value"),
        Some(0.0)
    );
    plugins.finalize();
}

#[test]
fn native_camera_writes_cancel_old_blends_only_after_validation() {
    let fixture = Fixture::new();
    fixture.write("bus.bus", "[model]\nmodel.cfg\n[mass]\n10000\n");
    fixture.write("model.cfg", "");
    let mut ty = omsi_sim::VehicleType::load(&fixture.0, &fixture.0.join("bus.bus")).unwrap();
    ty.def.cameras_driver = vec![
        omsi_vehicle::Camera {
            fov: 60.0,
            ..Default::default()
        },
        omsi_vehicle::Camera {
            yaw: 45.0,
            fov: 60.0,
            ..Default::default()
        },
    ];
    ty.def.camera_std = 0;
    let vehicle =
        omsi_sim::VehicleInstance::new(std::sync::Arc::new(ty), omsi_sim::VehicleHost::default());
    let mut app = new_app(
        Args::parse_from(["openomsi", "--root", fixture.0.to_str().unwrap()]),
        settings::Settings::default(),
    );
    app.player = Some(player(vehicle));
    app.view = "driver".into();
    app.camera = Some(omsi_render::Camera {
        position: DVec3::ZERO,
        yaw: 0.0,
        pitch: 0.0,
        roll: 0.0,
        fov_deg: 60.0,
        near: 0.2,
        far: 6000.0,
    });
    let active_blend = || crate::app::CamBlend {
        key: Some(("driver".into(), (0, 0))),
        from: Some(omsi_vehicle::Camera {
            yaw: 90.0,
            ..Default::default()
        }),
        shown: Some(omsi_vehicle::Camera {
            yaw: 30.0,
            ..Default::default()
        }),
        entering: true,
        t: 0.5,
        carry: Some(crate::app::CamCarry {
            pos: DVec3::X,
            yaw: 5.0,
            pitch: 2.0,
            roll: 3.0,
            fov: 4.0,
        }),
    };
    let assert_active = |blend: &crate::app::CamBlend| {
        let (view, choice) = blend.key.as_ref().unwrap();
        assert_eq!(view, "driver");
        assert_eq!(*choice, (0, 0));
        assert_eq!(blend.from.as_ref().unwrap().yaw, 90.0);
        assert_eq!(blend.shown.as_ref().unwrap().yaw, 30.0);
        assert!(blend.entering);
        assert_eq!(blend.t, 0.5);
        let carry = blend.carry.as_ref().unwrap();
        assert_eq!(carry.pos, DVec3::X);
        assert_eq!(
            (carry.yaw, carry.pitch, carry.roll, carry.fov),
            (5.0, 2.0, 3.0, 4.0)
        );
    };
    let assert_cleared = |blend: &crate::app::CamBlend| {
        assert!(blend.key.is_none() && blend.from.is_none() && blend.shown.is_none());
        assert!(!blend.entering && blend.carry.is_none());
        assert_eq!(blend.t, 0.0);
    };

    app.cam_blend = active_blend();
    assert!(plugin_api::execute(
        &mut app,
        "camera.select",
        serde_json::json!({"mode":"driver","index":999}),
        &[]
    )
    .is_err());
    assert_active(&app.cam_blend);
    assert_eq!(app.player.as_ref().unwrap().cam_choice.0, 0);
    plugin_api::execute(
        &mut app,
        "camera.select",
        serde_json::json!({"mode":"driver","index":1}),
        &[],
    )
    .unwrap();
    assert_cleared(&app.cam_blend);
    assert_eq!(app.player.as_ref().unwrap().cam_choice.0, 1);

    // A look update during a switch must not resume its old source/carry next frame.
    app.cam_blend = active_blend();
    assert!(plugin_api::execute(
        &mut app,
        "camera.set",
        serde_json::json!({"look_yaw_degrees":20,"fov_degrees":999}),
        &[]
    )
    .is_err());
    assert_active(&app.cam_blend);
    assert_eq!(app.look.0, 0.0);
    plugin_api::execute(
        &mut app,
        "camera.set",
        serde_json::json!({"look_yaw_degrees":20,"fov_degrees":75}),
        &[],
    )
    .unwrap();
    assert_cleared(&app.cam_blend);
    assert_eq!(app.look.0, 20.0);
    assert!((app.camera.unwrap().fov_deg - 75.0).abs() < 0.0001);
}
