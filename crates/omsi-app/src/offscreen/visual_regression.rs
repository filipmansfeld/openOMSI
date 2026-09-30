//! Opt-in installed-content rendering without normal startup, plugin discovery,
//! LAN cleanup, personnel saves or situation saves. Run alone on a GPU machine.
use super::*;

#[test]
#[ignore = "requires installed Praha 200 assets, a GPU and explicit output directory"]
fn installed_praha_crossing_visual_regression() {
    let root = PathBuf::from(std::env::var_os("OMSI_TEST_CONTENT")
        .expect("set OMSI_TEST_CONTENT to the installed original content root"));
    let output = PathBuf::from(std::env::var_os("OMSI_VISUAL_OUTPUT")
        .expect("set OMSI_VISUAL_OUTPUT to a validation output directory outside game content"));
    assert!(output.is_absolute(),"visual output must be an absolute path");
    assert!(output.is_dir(),"create the validation output directory before running this test");
    let output = output.canonicalize().unwrap();
    let root = root.canonicalize().unwrap();
    assert!(!output.starts_with(&root),"output must be outside installed game content");
    let overlay = std::env::var_os("OMSI_VISUAL_OVERLAY").map(PathBuf::from)
        .map(|path|path.canonicalize().unwrap());
    if let Some(overlay) = &overlay {
        assert!(!output.starts_with(overlay),"output must be outside the content overlay");
        omsi_cfg::add_content_root(overlay.clone());
    }
    omsi_cfg::add_content_root(root.clone());
    for name in ["OMSI_GROUND_SAMPLE","OMSI_DUMP_GROUND","OMSI_SUSP_TRACE",
        "OMSI_ROAD_PHOTO","OMSI_POPULATION_SHOTS","OMSI_DUMP_SCRIPTTEX"] {
        assert!(std::env::var_os(name).is_none(),"unset optional diagnostic write hook {name}");
    }
    let map = omsi_cfg::resolve_path(&root,"maps/Praha 200/global.cfg");
    assert!(omsi_cfg::vfs::is_file(&map),"Praha 200 must be installed");
    let mesh = omsi_cfg::resolve_path(&root,"Sceneryobjects/D016_semafory/model/semafor_3svetla.X");
    let housing = omsi_o3d::load_mesh(&mesh).unwrap();
    assert!(!housing.triangles.is_empty());
    assert!(housing.materials.iter().all(|material|material.texture.eq_ignore_ascii_case("semafor_zaklad.bmp")),
        "the crossing light housing must resolve its referenced X materials before rendering");

    // Use a real light on the crossing, including the production tile scaling
    // and x/height/y spline parser. The saved free camera was only ~11 m above
    // the street and looked into the park north of this crossing.
    let global = omsi_map::GlobalCfg::load(&map).unwrap();
    omsi_map::configure_grid(&global);
    let tile_ref = global.tiles.iter().find(|t|t.x==2641 && t.y==10560).unwrap();
    let tile_path = omsi_cfg::resolve_path(map.parent().unwrap(),&tile_ref.file);
    let tile = crate::tiles::read_tile(&tile_path,&[]).unwrap();
    let lamp = tile.spline_attachments.iter().find(|a|a.id==66597).unwrap();
    let tile_origin = glam::DVec2::new(tile_ref.x as f64,tile_ref.y as f64)*omsi_map::tile_size();
    let placed = crate::tiles::tile_row_objects(lamp,&tile.splines,tile_origin,None);
    let light_pose = placed.first().expect("reference light 66597 must actually be placed").1.pose;
    let target = light_pose.pos + glam::DVec3::new(-5.0,10.0,-lamp.offset[1]);
    let eye = target + glam::DVec3::new(44.0,-50.0,58.0);
    let direction = target-eye;
    let yaw = direction.x.atan2(direction.y).to_degrees().rem_euclid(360.0);
    let pitch = direction.z.atan2(direction.truncate().length()).to_degrees();
    let camera = std::env::var("OMSI_VISUAL_CAMERA").unwrap_or_else(|_|
        format!("{},{},{},{yaw},{pitch},60",eye.x,eye.y,eye.z));
    let time = std::env::var("OMSI_VISUAL_TIME").unwrap_or_else(|_|"12:00".into());
    let (hour,minute)=time.split_once(':').expect("OMSI_VISUAL_TIME must be HH:MM");
    assert!(hour.len()==2 && minute.len()==2 && hour.bytes().chain(minute.bytes()).all(|c|c.is_ascii_digit()),
        "OMSI_VISUAL_TIME must contain only HH:MM digits");
    assert!(hour.parse::<u32>().unwrap()<24 && minute.parse::<u32>().unwrap()<60,
        "OMSI_VISUAL_TIME must be a valid clock time");
    let stem = format!("praha-crossing-{}",time.replace(':',"-"));
    let image_path = output.join(format!("{stem}.png"));
    let args = Args::parse_from([
        "openomsi", "--root", root.to_str().unwrap(), "--map", "maps/Praha 200/global.cfg",
        "--offscreen",image_path.to_str().unwrap(),"--size","1600x900",
        "--cam",&camera,"--radius","1","--view","free",
        "--date","1989-05-30","--time",&time,"--traffic","0",
    ]);
    assert!(args.bus.is_none() && args.driver.is_none() && args.save_situation.is_none());
    assert!(args.lan_host.is_none() && args.lan_join.is_none() && args.server.is_none());
    assert!(!args.schedule && !args.passengers);
    super::run_offscreen(&args,&image_path,None,lan::LanGame::default()).unwrap();
    let rendered = image::open(&image_path).unwrap().to_rgb8();
    assert_eq!(rendered.dimensions(),(1600,900));
    let mut brightness = rendered.pixels().map(|pixel|pixel.0.iter().map(|v|*v as u16).sum::<u16>());
    let first = brightness.next().unwrap();
    let (minimum,maximum) = brightness.fold((first,first),|(lo,hi),value|(lo.min(value),hi.max(value)));
    assert!(maximum.saturating_sub(minimum)>40,"rendered image is unexpectedly blank");
    let record = serde_json::json!({
        "content_root":root,"overlay":overlay,"map":map,"camera":camera,
        "date":"1989-05-30","time":time,"image":image_path,"image_size":[1600,900],
        "light_housing_mesh":mesh,"housing_triangles":housing.triangles.len(),
        "reference_light_map_id":"66597","reference_light_position":light_pose.pos.to_array(),
        "reference_tile":tile_path,"camera_aim_target":target.to_array(),
        "nearby_tile_edges_m":{"x":tile_origin.x,"y":tile_origin.y+omsi_map::tile_size()},
        "verification":"Image generated from production offscreen renderer; visual inspection still required. This does not prove every road/terrain seam is fixed."
    });
    std::fs::write(output.join(format!("{stem}.json")),serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    println!("visual regression image: {}",image_path.display());
}
