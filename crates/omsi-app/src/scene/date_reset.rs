//! Regression fixtures for a calendar transition that replaces the native road graph.
use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("openomsi-date-reset-{}-{nonce}",std::process::id()));
        std::fs::create_dir_all(path.join("Chrono/later")).unwrap();
        std::fs::write(path.join("global.cfg"),"[name]\nDate reset fixture\n[ticketpack]\nTicketPacks/Before.otp\n").unwrap();
        std::fs::write(path.join("ailists.cfg"),concat!(
            "[aigroup_depot]\nDepot\nBefore\n",
            "[aigroup_depot_typgroup_2]\nVehicles/test.bus\n",
            "1\tOLD\tOld\t19800101\t19891231\n",
            "2\tNEW\tNew\t19900101\t20001231\n[end]\n",
        )).unwrap();
        std::fs::write(path.join("Chrono/later/Chrono.cfg"),"[startdate]\n19900101\n[ticketpack]\nTicketPacks/After.otp\n").unwrap();
        std::fs::write(path.join("Chrono/later/ailists_#upd.cfg"),"[aigroup_depot]\nDepot\nAfter\n").unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

#[test]
fn date_reset_preflight_preserves_live_graph_then_reseeds_clean_generation() {
    let fixture = Fixture::new();
    let world = World::open(&fixture.0,&fixture.0.join("global.cfg"),19891231).unwrap();
    let first_lists = world.ai_lists();
    assert_eq!(world.ticket_pack(),"TicketPacks/Before.otp");
    assert_eq!(world.ticket_pack_on_date(19900101),"TicketPacks/After.otp");
    assert_eq!(first_lists.groups[0].hof.as_deref(),Some("Before"));
    assert_eq!(first_lists.groups[0].typgroups[0].entries[0].number,"1");
    let future = world.ai_lists_on_date(19900101);
    assert_eq!(future.groups[0].hof.as_deref(),Some("After"));
    assert_eq!(future.groups[0].typgroups[0].entries[0].number,"2");
    assert_eq!(world.ai_lists().groups[0].hof.as_deref(),Some("Before"));

    let lane = || LaneBuilder::arc(DVec3::ZERO,0.0,20.0,0.0,0.0,LaneKind::Street,3.0);
    world.lanes.lock().push(lane());
    world.lane_tiles.lock().push((1,2));
    world.seeded.lock().insert((1,2));
    world.traffic_lights.lock().push(TrafficLightController::new(vec![vec![(0,30.0)]],30.0));
    world.controller_of_object.lock().insert(55,0);
    let candidate = crate::traffic::Traffic::prepare_date_reset(&fixture.0,&world,7,19900101).unwrap();
    let other = crate::traffic::Traffic::prepare_date_reset(&fixture.0,&world,7,19900101).unwrap();
    assert!(candidate.net.lanes.is_empty());
    assert!(candidate.api_light_controllers().is_empty());
    assert!(candidate.api_light_controller_index(55).is_none());
    assert_ne!(candidate.api_generation,other.api_generation);
    assert_eq!(candidate.target,7);
    assert_eq!(world.lanes.lock().len(),1); // preflight cannot take the live queue
    assert_eq!(world.traffic_lights.lock().len(),1);

    let old_source_generation = world.preparation_generation();
    world.set_date(19900101);
    world.reset_traffic_sources();
    assert_eq!(world.ai_lists().groups[0].hof.as_deref(),Some("After"));
    assert_eq!(world.ticket_pack(),"TicketPacks/After.otp");
    assert!(world.lanes.lock().is_empty());
    assert!(world.lane_tiles.lock().is_empty());
    assert!(world.seeded.lock().is_empty());
    assert!(world.traffic_lights.lock().is_empty());
    assert!(world.controller_of_object.lock().is_empty());
    assert!(world.layout.lock().is_none());
    assert!(world.index.lock().is_none());
    // A worker scheduled before the transition but starting after it may not
    // rebuild even the index/layout caches from its obsolete source generation.
    let (prepared,_) = world.prepare_tiles_for_generation(&[],old_source_generation);
    assert!(prepared.is_empty());
    assert!(world.index.lock().is_none());
    assert!(world.layout.lock().is_none());
    let current_source_generation = world.preparation_generation();
    assert_ne!(old_source_generation,current_source_generation);
    let _ = world.prepare_tiles_for_generation(&[],current_source_generation);
    assert!(world.index.lock().is_some());
    assert!(world.layout.lock().is_some());

    let mut active = candidate;
    world.lanes.lock().push(lane());
    world.lane_tiles.lock().push((9,10));
    world.traffic_lights.lock().push(TrafficLightController::new(vec![vec![(1,15.0)]],15.0));
    world.controller_of_object.lock().insert(99,0);
    assert_eq!(active.add_tiles(&world),1);
    assert_eq!(active.net.lanes.len(),1);
    assert_eq!(active.api_light_controllers().len(),1);
    assert!(active.api_light_controller_index(99).is_some());
    assert!(active.api_light_controller_index(55).is_none());
    assert!(!active.lane_tiles.contains(&(1,2)));
    assert!(active.lane_tiles.contains(&(9,10)));
}
