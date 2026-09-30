//! Live, main-thread placement changes. Classes with graph/terrain/light ownership
//! require a rebuild and are deliberately rejected before any state is changed.
use super::*;

fn movable_type(ty: &ObjectType) -> Result<(), String> {
    let s = &ty.sco;
    let m = &ty.model;
    if s.surface || s.render_type != omsi_scenery::sco::RenderType::Normal
        || !ty.holes.is_empty() || ty.deform.is_some() || !s.paths.is_empty()
        || s.paths_file.is_some() || !s.spline_helpers.is_empty()
        || s.is_bus_stop || s.is_entry_point || s.is_car_park || s.is_depot
        || s.passenger_cabin.is_some() || s.is_petrol_station || s.tree.is_some()
        || s.crash_mode_pole.is_some() || s.is_traffic_light || s.is_signal
        || !s.traffic_lights.is_empty() || !s.trigger_boxes.is_empty()
        || !s.rail_enh.is_empty() || !s.third_rail.is_empty()
    {
        return Err("this placement owns terrain, traffic, passenger or other spatial state that requires a rebuild".into());
    }
    if s.sound.is_some() || s.sound_ai.is_some() || !m.smokes.is_empty()
        || !m.particle_emitters.is_empty() || !s.map_lights.is_empty()
        || !m.lights.is_empty() || !m.spotlights.is_empty() || !m.interior_lights.is_empty()
        || !s.reflexion_cameras.is_empty()
        || m.meshes.iter().any(|m| !m.light_enh.is_empty() || !m.light_enh_2.is_empty())
    {
        return Err("placement lights, audio, particles and reflection cameras cannot yet be relocated coherently".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("openomsi-placement-{}-{nonce}",std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("global.cfg"),"[name]\nPlacement test\n").unwrap();
            std::fs::write(path.join("object.sco"),"[friendlyname]\nFixture\n").unwrap();
            Self(path)
        }
        fn world(&self) -> World { World::open(&self.0,&self.0.join("global.cfg"),19890530).unwrap() }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    fn object(world:&World) -> EditObject {
        let ty = world.object_type("object.sco").unwrap();
        EditObject {api_id:701,tile:(0,0),pos:DVec3::new(20.0,25.0,3.0),xf:Mat4::IDENTITY,
            key:77,instances:vec![10,11],sco:ty.sco.path.clone(),ty,native_origin:None,terrain_mapped:false}
    }

    #[test]
    fn live_placement_moves_collision_camera_script_and_index_together() {
        let fixture = Fixture::new();
        let world = fixture.world();
        let object = object(&world);
        let mut obstacle = omsi_sim::collision::Obb::from_box([2.0,4.0,5.0,1.0,0.0,2.5],object.pos,0.0);
        obstacle.id = object.key;
        let mut other = obstacle;
        other.id = 88;
        let shape = Arc::new(omsi_sim::collision::MeshShape::from_triangles(
            [[DVec3::new(-1.0,0.0,0.0),DVec3::new(1.0,0.0,0.0),DVec3::new(1.0,0.0,4.0)]].into_iter(),0.0));
        let mut state = TileState::default();
        state.obstacles.extend([obstacle,other]);
        state.mesh_obstacles.push(omsi_sim::collision::MeshObstacle::new(shape,object.pos,0.0,77));
        state.blockers = [77,88].into_iter().map(|object_key|crate::camera_arm::Blocker {
            object_key,ty:Arc::downgrade(&object.ty),pos:object.pos,xf:object.xf,radius:10.0,
        }).collect();
        world.tile_state.lock().insert((0,0),state);
        world.object_positions.lock().insert(7,(object.pos,[0.0;3]));
        world.edit_objects.lock().insert(7,object.clone());
        let mut program = omsi_script::Program::default();
        program.declare_var("value");
        let mut inst = omsi_sim::scenery::SceneryInstance::new(Arc::new(program),&[],Default::default(),&[]);
        inst.set_var("value",37.0);
        world.scripted.lock().push(ScriptedObject {
            api_id:702,ty:object.ty.clone(),pos:object.pos,xf:object.xf,instances:vec![10],inst,
            controller:None,light_index:0,map_id:7,variants:vec![],sounds:None,tile:(0,0),
            var_parent:None,texts:vec![],arrivals:false,htmls:vec![],
        });
        let to = DVec3::new(120.0,125.0,8.0);
        let movement = world.prepare_placement_move(701,Some(to),Some(90.0)).unwrap();
        // An animated mesh's local translation survives the rigid parent change.
        let pivot = Mat4::from_translation(glam::Vec3::new(2.0,3.0,1.0));
        let local = (movement.rotation*pivot).transform_point3(glam::Vec3::ZERO);
        assert!((local-glam::Vec3::new(3.0,-2.0,1.0)).length()<1e-5);
        world.commit_placement_move(&movement);
        let states = world.tile_state.lock();
        let st = &states[&(0,0)];
        assert!((st.obstacles[0].center-glam::DVec2::new(120.0,124.0)).length()<1e-6);
        assert_eq!(st.obstacles[0].z0,8.0);
        assert_eq!(st.obstacles[1].center,other.center);
        assert_eq!(st.mesh_obstacles[0].pos,to);
        assert!((st.mesh_obstacles[0].heading-std::f64::consts::FRAC_PI_2).abs()<1e-6);
        assert_eq!(st.blockers[0].pos,to);
        assert_eq!(st.blockers[1].pos,object.pos);
        drop(states);
        assert_eq!(world.object_positions.lock()[&7].0,to);
        assert_eq!(world.scripted.lock()[0].pos,to);
        assert_eq!(world.scripted.lock()[0].inst.var("value"),Some(37.0));
        assert!(world.collision.lock().obstacles_near(&obstacle).iter().all(|b|b.id!=77));
        let at_new = omsi_sim::collision::Obb::from_box([3.0,5.0,6.0,0.0,0.0,3.0],to,90.0);
        assert!(world.collision.lock().obstacles_near(&at_new).iter().any(|b|b.id==77));
        let back = world.prepare_placement_move(701,Some(object.pos),Some(0.0)).unwrap();
        world.commit_placement_move(&back);
        assert_eq!(world.edit_objects.lock()[&7].native_origin.unwrap().pos,object.pos);
        world.commit_placement_move(&world.prepare_placement_move(701,Some(to),None).unwrap());
        world.forget_native_placements((0,0));
        assert_eq!(world.object_positions.lock()[&7].0,object.pos);
        // Chrono invalidation wins over restoration of an old loaded pose.
        world.object_positions.lock().remove(&7);
        world.forget_native_placements((0,0));
        assert!(!world.object_positions.lock().contains_key(&7));
    }

    #[test]
    fn invalid_stale_dependent_and_editor_placements_do_not_partially_move() {
        let fixture = Fixture::new();
        let world = fixture.world();
        let object = object(&world);
        world.edit_objects.lock().insert(7,object.clone());
        world.tile_state.lock().insert((0,0),TileState::default());
        for (id,pos,heading) in [(999,Some(DVec3::ONE),None),(701,Some(DVec3::splat(f64::NAN)),None),
            (701,Some(DVec3::new(-1.0,2.0,0.0)),None),(701,None,Some(f64::INFINITY)),(701,None,None)] {
            assert!(world.prepare_placement_move(id,pos,heading).is_err());
        }
        let mut index = MapIndex::default();
        index.placement_references.insert(7);
        *world.index.lock() = Some(Arc::new(index));
        assert!(world.prepare_placement_move(701,Some(DVec3::ONE),None).err().unwrap().contains("depends"));
        *world.index.lock() = Some(Arc::new(MapIndex::default()));
        world.object_edits.lock().insert(7,ObjectEdit::default());
        assert!(world.prepare_placement_move(701,Some(DVec3::ONE),None).is_err());
        world.object_edits.lock().clear();
        world.edit_objects.lock().get_mut(&7).unwrap().terrain_mapped = true;
        assert!(world.prepare_placement_move(701,Some(DVec3::ONE),None).is_err());
        assert_eq!(world.edit_objects.lock()[&7].pos,object.pos);
        assert!(world.edit_objects.lock()[&7].native_origin.is_none());
    }

    #[test]
    fn map_index_retains_dependencies_on_unloaded_tiles_and_duplicate_source_ids() {
        let fixture = Fixture::new();
        let object = "[object]\n0\nobject.sco\n7\n20\n25\n0\n0\n0\n0\n0\n0\n";
        let a = fixture.0.join("tile_0_0.map");
        let b = fixture.0.join("tile_1_0.map");
        std::fs::write(&a,object).unwrap();
        std::fs::write(&b,format!("{object}[attachObj]\n0\nobject.sco\n8\n7\n0\n0\n0\n0\n0\n0\n0\n")).unwrap();
        let index = MapIndex::build(&[(0,0,0,a),(1,1,0,b)],&[]);
        assert!(index.placement_references.contains(&7));
        assert!(index.placement_duplicates.contains(&7));
        assert_eq!(index.tiles_read,2);
    }

    #[test]
    fn placement_classes_with_external_spatial_owners_are_rejected() {
        let fixture = Fixture::new();
        let world = fixture.world();
        assert!(movable_type(&world.object_type("object.sco").unwrap()).is_ok());
        for (name,definition) in [("stop","[busstop]\n"),("sound","[sound]\nambient.cfg\n"),
            ("surface","[surface]\n"),("light","[trafficlight]\n"),
            ("waiting","[passengercabin]\nwaiting.cfg\n")] {
            let name = format!("{name}.sco");
            std::fs::write(fixture.0.join(&name),definition).unwrap();
            assert!(movable_type(&world.object_type(&name).unwrap()).is_err(),"{name}");
        }
    }
}

struct PlacementMove {
    map_id: i64,
    object: EditObject,
    to: Pose,
    turn: f64,
    rotation: Mat4,
}

impl PlacementMove {
    fn point(&self, p: DVec3) -> DVec3 {
        let d = p - self.object.pos;
        let (s,c) = self.turn.sin_cos();
        self.to.pos + DVec3::new(d.x*c+d.y*s, -d.x*s+d.y*c, d.z)
    }

    fn obb(&self, b: &mut omsi_sim::collision::Obb) {
        b.center = self.point(b.center.extend(self.object.pos.z)).truncate();
        b.heading += self.turn;
        let dz = self.to.pos.z - self.object.pos.z;
        b.z0 += dz;
        b.z1 += dz;
    }
}

impl World {
    fn prepare_placement_move(&self, api_id: u64, position: Option<DVec3>, heading: Option<f64>)
        -> Result<PlacementMove,String>
    {
        if position.is_none() && heading.is_none() { return Err("set needs position or heading_deg".into()); }
        if position.is_some_and(|p| !p.is_finite()) || heading.is_some_and(|h| !h.is_finite()) {
            return Err("placement pose must contain finite numbers".into());
        }
        let (map_id,object) = self.edit_objects.lock().iter().find(|(_,o)|o.api_id==api_id)
            .map(|(id,o)|(*id,o.clone())).ok_or("placement handle is stale or not loaded")?;
        let index = self.index();
        if index.tiles_failed != 0 { return Err("the complete map dependency index is unavailable".into()); }
        if index.placement_duplicates.contains(&map_id) || self.object_dups.lock().keys().any(|(_,id)|*id==map_id) {
            return Err("placement source identity is ambiguous".into());
        }
        if index.placement_references.contains(&map_id) {
            return Err("an attached or variable-parent object depends on this placement".into());
        }
        if self.object_edits.lock().contains_key(&map_id) {
            return Err("an editor-modified placement cannot also be relocated by a native plugin".into());
        }
        movable_type(&object.ty)?;
        if object.terrain_mapped { return Err("terrain-mapped or warped placement requires a geometry rebuild".into()); }
        if !self.tile_state.lock().contains_key(&object.tile) {
            return Err("placement tile is no longer loaded".into());
        }
        if self.scripted.lock().iter().filter(|o|o.map_id==map_id && o.tile==object.tile).count()>1 {
            return Err("placement has ambiguous scripted instances".into());
        }
        let pos = position.unwrap_or(object.pos);
        let ts = tile_size();
        // GPU/light-map ownership remains with the loaded source tile. A cross-tile
        // transfer needs streaming/resource migration, not just a transform write.
        if pos.x < object.tile.0 as f64*ts || pos.x >= (object.tile.0 as f64+1.0)*ts
            || pos.y < object.tile.1 as f64*ts || pos.y >= (object.tile.1 as f64+1.0)*ts
            || (pos.z-object.pos.z).abs()>10_000.0
        {
            return Err("placement must remain inside its source tile and within 10000 m vertically".into());
        }
        let previous = Pose { pos:object.pos,rot:object.xf }.heading();
        let turn = (heading.unwrap_or(previous)-previous+180.0).rem_euclid(360.0)-180.0;
        let turn = turn.to_radians();
        let rotation = Mat4::from_rotation_z(-turn as f32);
        let to = Pose {pos,rot:rotation*object.xf};
        Ok(PlacementMove {map_id,object,to,turn,rotation})
    }

    fn commit_placement_move(&self, movement: &PlacementMove) {
        let o = &movement.object;
        if let Some(st) = self.tile_state.lock().get_mut(&o.tile) {
            for b in st.obstacles.iter_mut().filter(|b|b.id==o.key) { movement.obb(b); }
            for mesh in st.mesh_obstacles.iter_mut().filter(|b|b.id==o.key) {
                mesh.pos = movement.point(mesh.pos);
                mesh.heading += movement.turn;
                movement.obb(&mut mesh.bounds);
            }
            for b in st.blockers.iter_mut().filter(|b|b.object_key==o.key) {
                b.pos = movement.point(b.pos);
                b.xf = movement.rotation*b.xf;
            }
        }
        for script in self.scripted.lock().iter_mut().filter(|s|s.map_id==movement.map_id && s.tile==o.tile) {
            script.pos = movement.to.pos;
            script.xf = movement.to.rot;
        }
        let pose = (movement.to.pos,[movement.to.heading(),0.0,0.0]);
        self.object_positions.lock().insert(movement.map_id,pose);
        if let Some(object) = self.edit_objects.lock().get_mut(&movement.map_id) {
            object.native_origin.get_or_insert(Pose {pos:object.pos,rot:object.xf});
            object.pos = movement.to.pos;
            object.xf = movement.to.rot;
        }
        self.refresh_tile_lists();
    }

    /// Move a live standalone ground object as a whole, preserving each mesh's
    /// current animation/pivot transform. Nothing is persisted to map files.
    pub(crate) fn bridge_set_placement(&self, renderer:&Renderer, scene:&mut Scene,
        api_id:u64, position:Option<DVec3>, heading:Option<f64>) -> Result<Pose,String>
    {
        let movement = self.prepare_placement_move(api_id,position,heading)?;
        let instances = movement.object.instances.iter().map(|&id| {
            let i = scene.instances.get(id).ok_or("placement render instance is stale")?;
            if i.ground_layer || i.surface || i.presurface || i.decal {
                return Err("terrain-mapped or surface instances require a geometry rebuild");
            }
            Ok((id,movement.point(i.origin),movement.rotation*i.transform))
        }).collect::<Result<Vec<_>,&str>>()?;
        if instances.is_empty() { return Err("placement has no live render instances".into()); }
        // All fallible validation precedes both the CPU and renderer writes.
        for (id,pos,xf) in instances { renderer.set_transform(scene,id,pos,xf); }
        self.commit_placement_move(&movement);
        Ok(movement.to)
    }

    pub(super) fn forget_native_placements(&self, tile:(i32,i32)) {
        let objects = self.edit_objects.lock();
        let mut positions = self.object_positions.lock();
        for (id,o) in objects.iter().filter(|(_,o)|o.tile==tile) {
            let Some(source) = o.native_origin else { continue };
            // A Chrono transition may already have invalidated/replaced this entry.
            if positions.get(id).is_some_and(|(p,_)|*p==o.pos) {
                positions.insert(*id,(source.pos,[source.heading(),0.0,0.0]));
            }
        }
    }
}
