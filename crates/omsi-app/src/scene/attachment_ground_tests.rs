//! CPU placement regressions; no installed content, GPU or process environment changes.
use super::*;

fn ground(height: f32) -> Terrain {
    Terrain { cells: 1, heights: vec![height; 4] }
}

fn object_type() -> Arc<ObjectType> {
    // Attachment operations are bare SCO subcommands, unlike [new_attachment].
    let sco = SceneryObject::parse(&omsi_cfg::CfgFile::from_str(
        "parent.sco", "[new_attachment]\nattach_trans\n2\n3\n4\n",
    ));
    assert_eq!(sco.attachments.len(), 1);
    assert_eq!(sco.attachments[0].ops, vec![("attach_trans".to_string(), vec![2.0, 3.0, 4.0])]);
    Arc::new(ObjectType {
        sco,
        sound_path: Default::default(),
        model: Model::default(),
        model_dir: PathBuf::new(),
        meshes: Vec::new(),
        mesh_visible: Vec::new(),
        mesh_def_index: Vec::new(),
        mesh_pivots: Vec::new(),
        mesh_shadow: Vec::new(),
        mesh_casts: Vec::new(),
        program: None,
        lower_lods: Vec::new(),
        lod0_min: 0.0,
        paint_scheme_count: 0,
        dynamic_textures: Vec::new(),
        holes: Vec::new(),
        deform: None,
        collision: None,
        camera: Default::default(),
        collision_shape: Default::default(),
    })
}

fn object(ot: &Arc<ObjectType>, id: i64, place: Placement) -> StagedObject {
    StagedObject {
        ot: ot.clone(), id, place, rules: Vec::new(), extra: Vec::new(),
        lamp_parent: None, parked: false, map_object: true, instance: 0, key: id,
    }
}

fn tile(key: (i32, i32), base: Terrain, objects: Vec<StagedObject>) -> StagedTile {
    StagedTile {
        tx: key.0, ty: key.1,
        origin: DVec3::new(key.0 as f64 * tile_size(), key.1 as f64 * tile_size(), 0.0),
        path: PathBuf::new(), base_terrain: base, align: Vec::new(),
        hole_rims: Vec::new(), water: None, splines: Vec::new(),
        meshes: Mutex::new(Some(Vec::new())), drive: Vec::new(),
        lanes: Mutex::new(Vec::new()), street_points: Vec::new(), objects,
        anchors: Vec::new(), counts: LoadStats::default(), resolved: Default::default(),
    }
}

#[test]
fn parent_and_attached_chain_use_same_final_ground_across_tile_files() {
    let ot = object_type();
    let final_ground = Terrain { cells: 1, heights: vec![1.0, 2.0, 3.0, 4.0] };
    for destination_x in [1, -1] {
        let destination = (destination_x, 0);
        let origin = DVec3::new(destination_x as f64 * tile_size(), 0.0, 0.0);
        let point = origin + DVec3::new(0.2 * tile_size(), 0.3 * tile_size(), 0.25);
        let objects = || vec![
            object(&ot, 3, Placement::Attached { parent: 2, index: 0, rot: [0.0; 3] }),
            object(&ot, 2, Placement::Attached { parent: 1, index: 0, rot: [0.0; 3] }),
            object(&ot, 1, Placement::Ground { x: point.x, y: point.y, z: point.z, rot: [90.0, 0.0, 0.0] }),
            object(&ot, 4, Placement::Ground { x: point.x, y: point.y, z: point.z, rot: [0.0; 3] }),
        ];
        // Authored/base height deliberately differs from the evaluator's final height.
        let own = tile(destination, ground(11.0), objects());
        let neighbour = tile((destination_x - 1, 0), ground(27.0), objects());
        let height = final_ground.sample((point.x - origin.x) as f32, point.y as f32) as f64;
        for neighbour_first in [false, true] {
            let mut evaluated = 0;
            let mut resolve_neighbour = || World::resolve_object_poses(&neighbour, &ground(27.0), |key| {
                assert_eq!(key, destination);
                evaluated += 1;
                Some((origin, final_ground.clone()))
            });
            let (own_poses, neighbour_poses) = if neighbour_first {
                let result = resolve_neighbour();
                (World::resolve_object_poses(&own, &final_ground, |_| panic!("own ground is already evaluated")), result)
            } else {
                let result = World::resolve_object_poses(&own, &final_ground, |_| panic!("own ground is already evaluated"));
                (result, resolve_neighbour())
            };
            assert_eq!(evaluated, 1, "two parents on one neighbour share its evaluation");
            assert_eq!(own_poses.1, 0);
            assert_eq!(neighbour_poses.1, 0);
            for (a, b) in own_poses.0.iter().zip(&neighbour_poses.0) {
                let (a, b) = (a.unwrap(), b.unwrap());
                assert!((a.pos - b.pos).length() < 1e-6);
                assert!(a.rot.abs_diff_eq(b.rot, 1e-6));
            }
            let parent = neighbour_poses.0[2].unwrap();
            assert!((parent.pos.z - (point.z + height)).abs() < 1e-6);
            let child = neighbour_poses.0[1].unwrap();
            let grandchild = neighbour_poses.0[0].unwrap();
            assert!((child.pos - parent.pos - DVec3::new(3.0, -2.0, 4.0)).length() < 1e-6);
            assert!((grandchild.pos - child.pos - DVec3::new(3.0, -2.0, 4.0)).length() < 1e-6);
        }
    }
}

#[test]
fn neighbour_evaluator_requires_all_fixed_sources_in_either_batch_order() {
    let destination = (1, 0);
    let extra = (2, 0); // Shapes the neighbour but is not this object's file's source.
    let layout = TileLayout {
        paths: HashMap::new(),
        sources: [((0, 0), vec![(0, 0), destination]), (destination, vec![(0, 0), destination, extra])].into_iter().collect(),
    };
    for order in [[(0, 0), destination], [destination, (0, 0)]] {
        let mut staged: HashMap<_, _> = order.into_iter()
            .map(|key| (key, Arc::new(tile(key, ground(0.0), Vec::new())))).collect();
        assert!(World::complete_ground_sources(&layout, &staged, destination, |_| None).is_none());
        // A source tile resolved by another tile's cut may not have a prepared neighbour.
        // Load exactly the destination's missing dependencies before evaluating terrain.
        let mut loaded = Vec::new();
        let selected = World::complete_ground_sources(&layout, &staged, destination, |key| {
            loaded.push(key);
            Some(Arc::new(tile(key, ground(8.0), Vec::new())))
        }).unwrap();
        assert_eq!(loaded, vec![extra]);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[&extra].base_terrain, ground(8.0));
        staged.insert(extra, Arc::new(tile(extra, ground(8.0), Vec::new())));
        let already_staged = World::complete_ground_sources(&layout, &staged, destination, |_| panic!("all dependencies are held")).unwrap();
        assert_eq!(already_staged.len(), 3);
        for (key, source) in &selected {
            assert_eq!(source.base_terrain, already_staged[key].base_terrain);
        }
        assert!(World::complete_ground_sources(&layout, &staged, (9, 9), |_| panic!("outside map")).is_none());
    }
}

#[test]
fn missing_neighbour_keeps_border_fallback_for_parent_and_child() {
    let ot = object_type();
    let point = DVec3::new(tile_size() + 10.0, 25.0, 0.5);
    let st = tile((0, 0), ground(6.0), vec![
        object(&ot, 1, Placement::Ground { x: point.x, y: point.y, z: point.z, rot: [0.0; 3] }),
        object(&ot, 2, Placement::Attached { parent: 1, index: 0, rot: [0.0; 3] }),
    ]);
    let (poses, unresolved) = World::resolve_object_poses(&st, &st.base_terrain, |_| None);
    assert_eq!(unresolved, 0);
    assert_eq!(poses[0].unwrap().pos, point + DVec3::Z * 6.0);
    assert_eq!(poses[1].unwrap().pos, point + DVec3::new(2.0, 3.0, 10.0));
}
