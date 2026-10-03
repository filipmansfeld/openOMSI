//! Host-owned bus restrictions and timetable duties. Admission and updates are processed
//! on the session's single receiving thread; a duty is never reserved by a client snapshot.
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TourOccupancy {
    pub line: String,
    pub tour: String,
    pub player_id: u32,
    pub player_name: String,
}

pub fn tour_key(tour: &str) -> Option<(String, String)> {
    let (line, tour) = tour.split_once('/')?;
    let (line, tour) = (line.trim(), tour.trim());
    (!line.is_empty() && !tour.is_empty())
        .then(|| (line.to_ascii_lowercase(), tour.to_ascii_lowercase()))
}

pub fn same_tour(a: &str, b: &str) -> bool {
    match (tour_key(a), tour_key(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a.trim().is_empty() && b.trim().is_empty(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenial {
    pub vehicle: bool,
    pub reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct HostPolicy {
    vehicles: Vec<String>,
    pub exclusive_tours: bool,
    owners: HashMap<u32, TourOccupancy>,
}

impl HostPolicy {
    pub fn new(vehicles: &[String], exclusive_tours: bool) -> Self {
        Self {
            vehicles: vehicles.iter().map(|v| bus_key(v)).collect(),
            exclusive_tours,
            owners: HashMap::new(),
        }
    }
    pub fn active(&self) -> bool {
        self.exclusive_tours || !self.vehicles.is_empty()
    }
    pub fn allows_vehicle(&self, bus: &str) -> bool {
        bus.is_empty() || self.vehicles.is_empty() || self.vehicles.contains(&bus_key(bus))
    }
    pub fn release(&mut self, id: u32) {
        self.owners.remove(&id);
    }
    pub fn update(
        &mut self,
        id: u32,
        name: &str,
        bus: &str,
        requested: &str,
    ) -> Result<(), PolicyDenial> {
        if !self.allows_vehicle(bus) {
            self.release(id);
            return Err(PolicyDenial {
                vehicle: true,
                reason: "This bus is not allowed on this server. Choose one of its offered buses."
                    .into(),
            });
        }
        if bus.is_empty() && !requested.trim().is_empty() {
            self.release(id);
            return Err(PolicyDenial {
                vehicle: false,
                reason: "Choose a permitted bus before taking a timetable duty.".into(),
            });
        }
        if !self.exclusive_tours || requested.trim().is_empty() {
            self.release(id);
            return Ok(());
        }
        let Some(key) = tour_key(requested) else {
            self.release(id);
            return Err(PolicyDenial {
                vehicle: false,
                reason: "The duty must name both its line and tour.".into(),
            });
        };
        if let Some(owner) = self.owners.values().find(|o| {
            o.player_id != id && (o.line.to_ascii_lowercase(), o.tour.to_ascii_lowercase()) == key
        }) {
            let reason = format!(
                "Line {} tour {} is occupied by {}. Choose another duty.",
                owner.line, owner.tour, owner.player_name
            );
            self.release(id);
            return Err(PolicyDenial {
                vehicle: false,
                reason,
            });
        }
        let (line, tour) = requested.split_once('/').unwrap();
        self.owners.insert(
            id,
            TourOccupancy {
                line: line.trim().into(),
                tour: tour.trim().into(),
                player_id: id,
                player_name: name.into(),
            },
        );
        Ok(())
    }
    pub fn occupancy(&self) -> Vec<TourOccupancy> {
        let mut out: Vec<_> = self.owners.values().cloned().collect();
        out.sort_by(|a, b| (&a.line, &a.tour, a.player_id).cmp(&(&b.line, &b.tour, b.player_id)));
        out
    }
}

fn bus_key(bus: &str) -> String {
    bus.trim().replace('\\', "/").to_ascii_lowercase()
}

pub(crate) fn hex(text: &str) -> String {
    text.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn unhex(text: &str) -> Option<String> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}
pub(crate) fn encode_occupancy(tours: &[TourOccupancy]) -> String {
    tours
        .iter()
        .map(|t| {
            format!(
                "{}:{}:{}:{}",
                t.player_id,
                hex(&t.line),
                hex(&t.tour),
                hex(&t.player_name)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
pub(crate) fn decode_occupancy(text: &str) -> Option<Vec<TourOccupancy>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for row in text.split(',') {
        if out.len() >= 64 {
            return None;
        }
        let fields: Vec<_> = row.split(':').collect();
        if fields.len() != 4 {
            return None;
        }
        let item = TourOccupancy {
            player_id: fields[0].parse().ok()?,
            line: unhex(fields[1])?,
            tour: unhex(fields[2])?,
            player_name: unhex(fields[3])?,
        };
        if item.player_id == 0
            || item.line.len() > 256
            || item.tour.len() > 256
            || item.player_name.len() > 160
            || tour_key(&format!("{}/{}", item.line, item.tour)).is_none()
        {
            return None;
        }
        if out.iter().any(|p: &TourOccupancy| {
            p.player_id == item.player_id
                || (p.line.eq_ignore_ascii_case(&item.line)
                    && p.tour.eq_ignore_ascii_case(&item.tour))
        }) {
            return None;
        }
        out.push(item);
    }
    Some(out)
}

pub(crate) fn newer(a: u32, b: u32) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < (1 << 31)
}

#[derive(Default)]
pub(crate) struct SnapshotAssembly {
    pub sequence: u32,
    pub exclusive: bool,
    pub pieces: Vec<Option<String>>,
}
impl SnapshotAssembly {
    pub fn add(
        &mut self,
        sequence: u32,
        exclusive: bool,
        index: usize,
        count: usize,
        text: &str,
    ) -> Option<Vec<TourOccupancy>> {
        if count == 0 || count > 64 || index >= count || text.len() > 1000 {
            return None;
        }
        if self.pieces.is_empty() || newer(sequence, self.sequence) {
            *self = Self {
                sequence,
                exclusive,
                pieces: vec![None; count],
            };
        }
        if sequence != self.sequence || exclusive != self.exclusive || self.pieces.len() != count {
            return None;
        }
        self.pieces[index] = Some(text.into());
        let joined = self
            .pieces
            .iter()
            .map(|p| p.as_deref())
            .collect::<Option<Vec<_>>>()?
            .concat();
        decode_occupancy(&joined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> HostPolicy {
        HostPolicy::new(&["Vehicles/SOR/NB.bus".into()], true)
    }
    #[test]
    fn first_claim_wins_and_owner_can_repeat() {
        let mut p = policy();
        assert!(p.update(2, "Alice", "Vehicles/SOR/NB.bus", "136/1").is_ok());
        assert!(p.update(3, "Bob", "Vehicles/SOR/NB.bus", "136/1").is_err());
        assert!(p
            .update(2, "Alice", "Vehicles/SOR/NB.bus", " 136 /1 ")
            .is_ok());
        assert_eq!(p.occupancy().len(), 1);
    }
    #[test]
    fn duties_on_distinct_lines_are_distinct() {
        let mut p = policy();
        assert!(p.update(2, "Alice", "Vehicles/SOR/NB.bus", "136/1").is_ok());
        assert!(p.update(3, "Bob", "Vehicles/SOR/NB.bus", "139/1").is_ok());
    }
    #[test]
    fn changing_or_leaving_releases_previous_duty() {
        let mut p = policy();
        p.update(2, "Alice", "Vehicles/SOR/NB.bus", "136/1")
            .unwrap();
        p.update(2, "Alice", "Vehicles/SOR/NB.bus", "139/1")
            .unwrap();
        p.update(3, "Bob", "Vehicles/SOR/NB.bus", "136/1").unwrap();
        p.release(2);
        p.update(3, "Bob", "Vehicles/SOR/NB.bus", "139/1").unwrap();
        p.update(3, "Bob", "", "").unwrap();
        assert!(p.occupancy().is_empty());
    }
    #[test]
    fn strict_vehicle_change_clears_its_duty() {
        let mut p = policy();
        p.update(2, "Alice", "Vehicles/SOR/NB.bus", "136/1")
            .unwrap();
        assert!(
            p.update(2, "Alice", "Vehicles/Other.bus", "136/1")
                .unwrap_err()
                .vehicle
        );
        assert!(p.occupancy().is_empty());
        assert!(p.allows_vehicle("vehicles\\SOR\\NB.BUS"));
        assert!(!p.allows_vehicle("Mods/vehicles/SOR/NB.bus"));
        assert!(!p.allows_vehicle("BadVehicles/SOR/NB.bus"));
    }
    #[test]
    fn regular_lan_has_no_restrictions() {
        let mut p = HostPolicy::default();
        for id in [2, 3] {
            p.update(id, "Driver", "Vehicles/Anything.bus", "136/1")
                .unwrap();
        }
        assert!(!p.active());
        assert!(p.occupancy().is_empty());
    }
    #[test]
    fn snapshot_reassembles_reordered_chunks_and_ignores_old_sequence() {
        let tours = vec![TourOccupancy {
            line: "136".into(),
            tour: "1".into(),
            player_id: 2,
            player_name: "Řidič: \"A\"".into(),
        }];
        let text = encode_occupancy(&tours);
        let (a, b) = text.split_at(12);
        let mut s = SnapshotAssembly::default();
        assert!(s.add(3, true, 1, 2, b).is_none());
        assert_eq!(s.add(3, true, 0, 2, a), Some(tours));
        assert!(s.add(2, true, 0, 1, "").is_none());
        assert_eq!(s.add(4, true, 0, 1, ""), Some(Vec::new()));
    }
    #[test]
    fn malformed_snapshot_never_means_free() {
        assert!(decode_occupancy("2:31:3:zz").is_none());
        let mut s = SnapshotAssembly::default();
        assert!(s.add(1, true, 0, 1, "broken").is_none());
        assert!(s.add(1, true, 64, 65, "").is_none());
    }
    #[test]
    fn request_revisions_wrap_without_accepting_old_updates() {
        assert!(newer(0, u32::MAX));
        assert!(!newer(u32::MAX, 0));
        assert!(!newer(2, 2));
    }
}
