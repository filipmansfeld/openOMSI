//! Invalidate source positions whose Chrono tile records have changed.
use super::*;

pub(super) fn collect_ids(tile: &omsi_map::Tile, ids: &mut hashbrown::HashSet<i64>) {
    ids.extend(tile.objects.iter().chain(&tile.attach_objects).map(|object|object.id));
    ids.extend(tile.spline_attachments.iter().map(|attachment|attachment.id));
}

pub(super) fn invalidate(
    positions: &mut HashMap<i64,(DVec3,[f64;3])>,
    duplicates: &mut HashMap<((i32,i32),i64),(DVec3,[f64;3])>,
    tiles: &hashbrown::HashSet<(i32,i32)>,
    ids: &hashbrown::HashSet<i64>,
    complete: bool,
) {
    if complete {
        positions.retain(|id,_|!ids.contains(id));
        duplicates.retain(|(tile,_),_|!tiles.contains(tile));
    } else {
        // A missing old source makes selective invalidation impossible. Clearing
        // this lookup cache is preferable to retaining a deleted stop indefinitely.
        positions.clear();
        duplicates.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_move_and_deletion_do_not_keep_old_cached_positions() {
        let mut old = omsi_map::Tile::default();
        old.objects.push(omsi_map::MapObject { id:1,..Default::default() });
        old.attach_objects.push(omsi_map::MapObject { id:2,..Default::default() });
        old.spline_attachments.push(omsi_map::SplineAttachment { id:3,..Default::default() });
        let mut new = omsi_map::Tile::default();
        new.objects.push(omsi_map::MapObject { id:2,..Default::default() });
        new.objects.push(omsi_map::MapObject { id:4,..Default::default() });
        let mut ids = hashbrown::HashSet::new();
        collect_ids(&old,&mut ids);
        collect_ids(&new,&mut ids);
        let pose = (DVec3::new(1.0,2.0,3.0),[0.0;3]);
        let mut positions = HashMap::from_iter([(1,pose),(2,pose),(3,pose),(4,pose),(99,pose)]);
        let mut duplicates = HashMap::from_iter([(((10,20),2),pose),(((11,20),2),pose)]);
        invalidate(&mut positions,&mut duplicates,&hashbrown::HashSet::from_iter([(10,20)]),&ids,true);
        assert_eq!(positions.len(),1);
        assert_eq!(positions[&99],pose);
        assert!(!duplicates.contains_key(&((10,20),2)));
        assert!(duplicates.contains_key(&((11,20),2)));
        // The ordinary index insertion now accepts the newly placed stop.
        let moved = (DVec3::new(100.0,200.0,30.0),[90.0,0.0,0.0]);
        positions.entry(2).or_insert(moved);
        assert_eq!(positions[&2],moved);
        assert!(!positions.contains_key(&1));
    }

    #[test]
    fn unreadable_old_sources_cannot_leave_stale_stops() {
        let pose = (DVec3::ZERO,[0.0;3]);
        let mut positions = HashMap::from_iter([(1,pose)]);
        let mut duplicates = HashMap::from_iter([(((10,20),1),pose)]);
        invalidate(&mut positions,&mut duplicates,&Default::default(),&Default::default(),false);
        assert!(positions.is_empty());
        assert!(duplicates.is_empty());
    }
}
