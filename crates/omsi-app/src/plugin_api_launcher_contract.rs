//! Opt-in contract against the installed Tangenta SOR, independent of the
//! launcher transport, GPU, external servers and map startup.
use std::{path::PathBuf,sync::Arc};

// Audited from launcher/Tangentalauncher C# Get/SetVariable, GetStringVariable
// and generation/announcement write wrappers, excluding bin/obj and comments.
// `rychlost` belongs to the scenery radar and is not a vehicle variable.
const NUMERIC: &[&str] = &[
    "act_pos_based_on_time","am_vol","casodj","dalsispoj","dispecink","dispecinktextura",
    "door_2","eco","f1","hodiny","IBIS_busstop","intpctxt","jasp","jkz_den","kamerar",
    "kartavis","kartou","klimatizovano","kmcounter_m","kmenovalinka","linka","linkajr",
    "minuty","mpc_linka","mpc_linka1","mpc_poradi","mpc_trasa","MPC_trasa1","nastav",
    "nastavc","nastavlinku","navx","navy","nizkopodlazni","nove","play","poradijr",
    "prejezd","prihlaseno","ridser","rotace","sekundy","smer","SndExt_Radio1","spoj",
    "st_aktiv","stran","tile","trip","usb","Velocity","vondracek","vyhlaseno","vyska",
    "vzdalenostzast","vzdalenostzast1","xtile","ytile","zbyva","zoom",
];
const STRINGS: &[&str] = &[
    "0jr","1jr","32jr","act_busstop","act_busstopnext","dopravce","IBIS_terminus_namelcd",
    "JKZ_DS","JKZ_Odchylka","JKZ_priznak","number","typ",
];
const WRITES: &[&str] = &[
    "act_pos_based_on_time","casodj","dispecink","dispecinktextura","hodiny","jasp","kamerar",
    "kartavis","linka","linkajr","minuty","mpc_linka","mpc_linka1","mpc_trasa","MPC_trasa1",
    "nastav","nastavlinku","navx","navy","poradijr","prejezd","ridser","rotace","sekundy",
    "smer","spoj","tile","trip","vyhlaseno","vzdalenostzast","vzdalenostzast1","xtile","ytile",
];

#[test]
#[ignore = "requires the installed Tangenta SOR NB assets in OMSI_TEST_CONTENT"]
fn installed_sor_launcher_variables_and_ois_triggers() {
    let root = PathBuf::from(std::env::var_os("OMSI_TEST_CONTENT")
        .expect("set OMSI_TEST_CONTENT to the installed original content root"));
    if let Some(overlay) = std::env::var_os("OMSI_VISUAL_OVERLAY") {
        omsi_cfg::add_content_root(PathBuf::from(overlay));
    }
    omsi_cfg::add_content_root(root.clone());
    let path = omsi_cfg::resolve_path(&root,"Vehicles/SOR NB/SORNB18_2011.bus");
    let ty = Arc::new(omsi_sim::VehicleType::load(&root,&path).expect("load the actual SORNB18_2011.bus"));
    let diagnostics:Vec<_> = ty.program.errors.iter().map(ToString::to_string).collect();
    for diagnostic in &diagnostics { println!("script diagnostic: {diagnostic}"); }
    let mut vehicle = omsi_sim::VehicleInstance::new(ty.clone(),
        omsi_sim::VehicleHost::new(omsi_sim::SimClock::default()));
    let missing_numeric:Vec<_> = NUMERIC.iter().copied().filter(|n|vehicle.var(n).is_none()).collect();
    let missing_strings:Vec<_> = STRINGS.iter().copied().filter(|n|ty.program.str_var(n).is_none()).collect();
    println!("SOR launcher contract: {} numeric, {} strings, {} writes; missing numeric={missing_numeric:?}, strings={missing_strings:?}; {} compiler diagnostics",
        NUMERIC.len(),STRINGS.len(),WRITES.len(),diagnostics.len());
    assert!(missing_numeric.is_empty(),"undeclared required launcher variables: {missing_numeric:?}");
    assert!(missing_strings.is_empty(),"undeclared required launcher strings: {missing_strings:?}");

    // A typo in the current launcher's announcement cleanup is a real content
    // mismatch, not an engine variable that should be invented by the bridge.
    assert!(vehicle.var("vyhlaseni").is_none(),"re-audit the launcher if this bus adds vyhlaseni");
    assert!(!vehicle.set_var("vyhlaseni",0.0));
    println!("KNOWN CONTENT GAP: launcher announcement cleanup writes vyhlaseni; this SOR does not declare it (vyhlaseno is declared).");
    for (i,name) in WRITES.iter().enumerate() {
        let before = vehicle.var(name).unwrap();
        let value = i as f32+0.125;
        assert!(vehicle.set_var(name,value),"write {name}");
        assert_eq!(vehicle.var(&name.to_uppercase()),Some(value),"read after write {name}");
        assert!(vehicle.set_var(name,before));
    }
    for name in STRINGS {
        let id = ty.program.str_var(name).unwrap() as usize;
        let before = vehicle.state.str_vars[id].clone();
        assert_eq!(vehicle.str_var(name),before,"read native string {name}");
    }
    assert_eq!(vehicle.var("zoom"),Some(16.0),"actual OIS IBIS_init ran");
    assert_eq!(vehicle.str_var("mypoldisplej"),"cerna.bmp","actual OIS init selects its black freetex");
    for (trigger,value) in [("karta",1.0),("karta1",0.0),("karta2",2.0),("karta2_off",0.0)] {
        assert!(ty.program.trigger(trigger).is_some(),"missing OIS trigger {trigger}");
        assert!(vehicle.trigger(trigger),"run OIS trigger {trigger}");
        assert_eq!(vehicle.var("karta"),Some(value));
    }
    assert!(vehicle.set_var("stran",0.0) && vehicle.set_var("vyska",0.0));
    assert!(vehicle.trigger("pravo") && vehicle.trigger("hore"));
    assert_eq!(vehicle.var("stran"),Some(1.0));
    assert_eq!(vehicle.var("vyska"),Some(1.0));
    assert!(vehicle.trigger("centr"));
    assert_eq!(vehicle.var("stran"),Some(0.0));
    assert_eq!(vehicle.var("vyska"),Some(0.0));
    assert!(vehicle.trigger("velkaplus"));
    assert_eq!(vehicle.var("zoom"),Some(17.0));
    assert!(vehicle.trigger("velkaminus"));
    assert_eq!(vehicle.var("zoom"),Some(16.0));
    assert!(ty.program.trigger("JKZ_hlaseni_vyhlasit").is_some());
    for (index,size) in [(2,(1000,660)),(4,(420,336))] {
        let texture = vehicle.host.script_textures.get(index).expect("authored launcher script texture slot");
        assert_eq!((texture.width,texture.height),size);
        assert_eq!(texture.rgba.len(),(size.0*size.1*4) as usize);
    }
    // Parse errors must stay visible even if the narrow trigger checks above
    // happen to pass; a partially compiled bus is not full script compatibility.
    assert!(diagnostics.is_empty(),"SOR script compilation has {} diagnostics; inspect the printed report",diagnostics.len());
    println!("Actual SOR OIS card/GPS triggers, script texture slots and supported launcher variable roundtrips passed.");
}

struct BridgeFixture(PathBuf);

#[test]
#[ignore = "requires installed Praha 200 timetable, SOR NB and its Prague HOF in OMSI_TEST_CONTENT"]
fn installed_sor_praha_timetable_duty_and_ois_callbacks() {
    use clap::Parser;
    use omsi_script::Host;
    use serde_json::json;
    let root = PathBuf::from(std::env::var_os("OMSI_TEST_CONTENT").expect("set OMSI_TEST_CONTENT"));
    if let Some(overlay) = std::env::var_os("OMSI_VISUAL_OVERLAY") {
        omsi_cfg::add_content_root(PathBuf::from(overlay));
    }
    omsi_cfg::add_content_root(root.clone());
    let mut app = crate::new_app(crate::Args::parse_from(["openomsi","--root",root.to_str().unwrap()]),
        crate::settings::Settings::default());
    // A fixed date before Praha's supplied 2023/2024 diversions makes the source
    // catalog deterministic without editing the installed map or its Chrono files.
    app.clock.set_date(2020,1,6);
    let world = Arc::new(crate::scene::World::open(&root,
        &omsi_cfg::resolve_path(&root,"maps/Praha 200/global.cfg"),app.clock.date_code()).unwrap());
    let _ = world.index(); // source positions only; no streamed meshes or renderer
    let schedule = crate::schedule::Schedule::new(&root,&world,&app.clock);
    // Installed Praha 177 uses the original eight-line [station] records;
    // newer maps may instead use [station_typ2]. Compare their actual IDs.
    let stations = |trip:&omsi_timetable::Trip| -> Vec<i64> {
        if !trip.stations.is_empty() { return trip.stations.clone(); }
        trip.stations_legacy.iter().map(|record|record.first().expect("station record")
            .trim().parse::<i64>().expect("source station object ID")).collect()
    };
    let line_index = schedule.data.lines.iter().position(|line|line.name=="177").expect("actual Praha line 177");
    let line = &schedule.data.lines[line_index];
    let tour_index = line.tours.iter().position(|tour|tour.trips.iter().any(|entry|
        schedule.data.trip(&entry.trip).is_some_and(|trip|trip.line=="177" && stations(trip).len()>=3)))
        .expect("public Praha 177 tour with at least three stops");
    let tour = line.tours[tour_index].clone();
    let source_entry_index = tour.trips.iter().position(|entry|schedule.data.trip(&entry.trip)
        .is_some_and(|trip|trip.line=="177" && stations(trip).len()>=3)).unwrap();
    let entry = tour.trips[source_entry_index].clone();
    let source_trip_index = schedule.data.trips.iter().position(|trip|trip.name.eq_ignore_ascii_case(&entry.trip)).unwrap();
    let source_trip = schedule.data.trips[source_trip_index].clone();
    let source_stop_ids = stations(&source_trip);
    app.clock.time = entry.departure as f64*60.0;
    let ty = Arc::new(omsi_sim::VehicleType::load(&root,
        &omsi_cfg::resolve_path(&root,"Vehicles/SOR NB/SORNB18_2011.bus")).unwrap());
    assert!(ty.program.errors.is_empty(),"actual SOR must compile completely");
    let mut host = omsi_sim::VehicleHost::new(app.clock.clone());
    host.hof = Some(Arc::new(omsi_vehicle::hof::Hof::load(&omsi_cfg::resolve_path(&root,
        "Vehicles/SOR NB/Prague_Citybus_VMatrix_Urbanway.hof")).unwrap()));
    let mut vehicle = omsi_sim::VehicleInstance::new(ty,host);
    // Keep the bus away from real stops; only the requested start/skip commands
    // should change the duty in this contract, not proximity-based auto advance.
    vehicle.position = glam::DVec3::new(-10000.0,-10000.0,100.0);
    app.player = Some(crate::plugin_api_contract::player(vehicle));
    app.world = Some(world);
    app.schedule = Some(schedule);
    let session = crate::plugin_api::session(&mut app);
    let call = |app:&mut crate::App,op:&str,mut args:serde_json::Value| {
        args["session_id"] = json!(session);
        crate::plugin_api::execute(app,op,args,&[]).unwrap()
    };
    call(&mut app,"timetable.assign",json!({"line":"177","tour":tour.number}));
    let trip_index = app.duty.as_ref().unwrap().trips.iter().position(|trip|
        trip.name.eq_ignore_ascii_case(&entry.trip) && (trip.departure-entry.departure as f64*60.0).abs()<0.01).unwrap();
    call(&mut app,"timetable.start_at",json!({"trip_index":trip_index,"stop_index":0}));
    let planned = app.duty.as_ref().unwrap().trip().clone();
    assert_eq!(planned.stops.iter().map(|stop|stop.object_id).collect::<Vec<_>>(),source_stop_ids);
    assert!((planned.departure-entry.departure as f64*60.0).abs()<0.001,"TTL minutes become seconds");
    let check = |app:&mut crate::App,stop_index:usize| {
        let state = call(app,"timetable.get",json!({}));
        assert_eq!(state["active"],true);
        assert_eq!(state["line"],line_index);
        assert_eq!(state["tour"],tour_index);
        assert_eq!(state["trip"],source_trip_index);
        assert_eq!(state["tour_entry"],source_entry_index);
        let expected = &planned.stops[stop_index];
        assert!((state["next_stop_arrival"].as_f64().unwrap()-expected.arr).abs()<0.001);
        assert!((state["next_stop_time_to_depart"].as_f64().unwrap()-(expected.dep-app.clock.time)).abs()<0.001);
        let bus = &mut app.player.as_mut().unwrap().vehicle;
        assert_eq!(bus.host.tt_busstop_index,stop_index as i32);
        assert_eq!(bus.host.tt_stop_ids,source_stop_ids);
        for (name,value) in [("getttbusstoparr",expected.arr),("getttbusstopdep",expected.dep)] {
            let mut stack = omsi_script::Stacks::default();
            stack.push(stop_index as f32);
            bus.host.callback(name,0,&mut stack,&mut bus.state);
            assert!((stack.pop() as f64-value).abs()<0.02,"{name}: seconds and stop order");
        }
        // Execute both branches of the authored OIS block, not a replacement
        // test script. A real HOF route with a following stop selects departure;
        // its no-route branch selects arrival. This tests callback selection,
        // not geographic equivalence of that HOF route with the planned trip.
        let route = bus.host.hof.as_ref().unwrap().info_busstop_lists.iter()
            .position(|stops|stops.len()>stop_index+1).expect("actual Prague HOF route with following stop");
        let program = bus.ty.program.clone();
        let block = program.macro_block("IBIS_frame").expect("actual OIS IBIS_frame");
        for (route,time) in [(-1.0,expected.arr),(route as f32,expected.dep)] {
            for (name,value) in [("IBIS_busstop",stop_index as f32),("IBIS_RouteIndex",route),
                ("Tangenta_terminal_deviation_hold",0.0)] { assert!(bus.set_var(name,value)); }
            bus.vm.run_block(&program,block,&mut bus.state,&mut bus.host);
            assert!((bus.var("MPC_zastavka_cas").unwrap() as f64-time).abs()<0.02,
                "authored OIS must consume the current timetable stop time, route={route}");
        }
    };
    check(&mut app,0);
    call(&mut app,"timetable.skip_stop",json!({"index":2}));
    check(&mut app,2);
    assert!(crate::plugin_api::execute(&mut app,"timetable.skip_stop",
        json!({"session_id":session,"index":1}),&[]).is_err());
    assert_eq!(app.duty.as_ref().unwrap().next_stop,2,"failed backwards skip preserves progress");
    call(&mut app,"timetable.clear",json!({}));
    assert_eq!(call(&mut app,"timetable.get",json!({}))["active"],false);
    assert!(app.player.as_ref().unwrap().vehicle.host.tt_stops.is_empty());
    assert!(app.renderer.is_none() && app.scene.is_none() && app.lan.is_none()
        && app.audio.is_none() && app.tangenta_bridge.is_none());
    println!("Actual Praha 177/SOR duty assign/start/skip/clear, catalog indices, minute-to-second conversion and authored OIS timetable callbacks passed; no GPU, network or profile writes.");
}

impl BridgeFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("openomsi-sor-live-{}-{}",
            std::process::id(),crate::plugin_api::random_id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for BridgeFixture {
    fn drop(&mut self) {
        if let (Ok(path),Ok(root)) = (self.0.canonicalize(),std::env::temp_dir().canonicalize()) {
            if path.starts_with(&root) && path!=root { let _ = std::fs::remove_dir_all(path); }
        }
    }
}

/// A failing assertion/timeout can terminate only the subprocess this test owns.
struct OwnedClient(std::process::Child);
impl Drop for OwnedClient {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(),Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn bounded_child_output(mut pipe:impl std::io::Read) -> String {
    let mut kept = Vec::new();
    let mut part = [0u8;4096];
    let mut truncated = false;
    while let Ok(count) = pipe.read(&mut part) {
        if count==0 { break; }
        let available = (16_384usize).saturating_sub(kept.len());
        kept.extend_from_slice(&part[..count.min(available)]);
        truncated |= count>available;
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated { output.push_str("\n[remaining child output omitted]\n"); }
    output
}

#[test]
#[ignore = "requires installed SOR assets and the built production C# BridgeClient.Check DLL"]
fn installed_sor_launcher_live_bridge() {
    use clap::Parser;
    use std::{process::{Command,Stdio},time::{Duration,Instant}};
    let root = PathBuf::from(std::env::var_os("OMSI_TEST_CONTENT")
        .expect("set OMSI_TEST_CONTENT to the installed original content root"));
    let dll = PathBuf::from(std::env::var_os("OMSI_LAUNCHER_CONTRACT_DLL")
        .expect("set OMSI_LAUNCHER_CONTRACT_DLL to the built BridgeClient.Check.dll"));
    assert!(dll.is_absolute() && dll.is_file(),"the client contract DLL must exist at an absolute path");
    assert_eq!(dll.file_name().unwrap().to_string_lossy(),"BridgeClient.Check.dll");
    if let Some(overlay) = std::env::var_os("OMSI_VISUAL_OVERLAY") {
        omsi_cfg::add_content_root(PathBuf::from(overlay));
    }
    omsi_cfg::add_content_root(root.clone());
    let ty = Arc::new(omsi_sim::VehicleType::load(&root,
        &omsi_cfg::resolve_path(&root,"Vehicles/SOR NB/SORNB18_2011.bus")).unwrap());
    let mut app = crate::new_app(crate::Args::parse_from(["openomsi","--root",root.to_str().unwrap()]),
        crate::settings::Settings::default());
    app.clock.time = 12345.25;
    let mut vehicle = omsi_sim::VehicleInstance::new(ty,
        omsi_sim::VehicleHost::new(app.clock.clone()));
    vehicle.position = glam::DVec3::new(150.0,75.0,10.0);
    vehicle.heading = 30.0;
    let mut player = crate::plugin_api_contract::player(vehicle);
    player.uid = 18_000_000_000_000_000_001;
    player.head = glam::Vec3::new(0.125,0.25,0.5);
    player.head_vel = glam::Vec3::new(0.0625,0.125,0.25);
    let before:Vec<_> = ["kartavis","nastavlinku","spoj","kmenovalinka"].into_iter()
        .map(|name|(name,player.vehicle.var(name).expect("actual launcher field"))).collect();
    app.player = Some(player);
    assert!(app.world.is_none() && app.renderer.is_none() && app.scene.is_none()
        && app.lan.is_none() && app.audio.is_none());
    let fixture = BridgeFixture::new();
    let manifest = fixture.0.join("manifest.json");
    app.tangenta_bridge = Some(crate::tangenta_bridge::Bridge::start_for_test(&root,&manifest).unwrap());
    // Publish the App session and ready snapshot before the client sees manifest.
    crate::tangenta_bridge::poll(&mut app);
    let dotnet = std::env::var_os("OMSI_LAUNCHER_DOTNET")
        .or_else(||std::env::var_os("OMSI_LAUNCHER_CONTRACT_DOTNET"))
        .unwrap_or_else(|| {
            let x86 = PathBuf::from("C:/Program Files (x86)/dotnet/dotnet.exe");
            if cfg!(windows) && x86.is_file() { x86.into_os_string() } else { "dotnet".into() }
        });
    let mut command = Command::new(dotnet);
    command.arg(&dll).arg("--engine-contract").arg(&manifest)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = OwnedClient(command.spawn().expect("start the existing dotnet client contract runtime"));
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let stdout = std::thread::spawn(move ||bounded_child_output(stdout));
    let stderr = std::thread::spawn(move ||bounded_child_output(stderr));
    let started = Instant::now();
    let status = loop {
        crate::tangenta_bridge::poll(&mut app);
        if let Some(status) = child.0.try_wait().expect("inspect own client process") { break status; }
        if started.elapsed()>Duration::from_secs(60) {
            child.0.kill().expect("stop only this test's timed-out client");
            let _ = child.0.wait();
            panic!("production C# client contract timed out after 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout.join().expect("client stdout reader");
    let stderr = stderr.join().expect("client stderr reader");
    // Closing this isolated listener removes only the manifest with its own token.
    app.tangenta_bridge = None;
    assert!(!manifest.exists(),"the isolated manifest must be removed on bridge drop");
    assert!(status.success(),"production client failed ({status}):\n{stdout}\n{stderr}");
    assert!(stdout.contains("ENGINE CONTRACT PASS"),"client did not execute engine contract mode:\n{stdout}\n{stderr}");
    println!("{stdout}");
    assert_eq!(app.clock.time,12345.25,"client restores clock after exercising writes");
    for (name,value) in before {
        assert_eq!(app.player.as_ref().unwrap().vehicle.var(name),Some(value),
            "production engine retained the client's restored {name}");
    }
}
