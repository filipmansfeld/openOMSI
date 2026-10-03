//! Instance-owned, bounded observations of real cockpit pointer input.
use std::collections::VecDeque;

const CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub struct VehicleInputEvent {
    pub sequence: u64,
    pub section: usize,
    pub trigger: String,
    pub mesh: String,
    pub pressed: bool,
    pub simulation_seconds: f64,
    pub uv: Option<[f32; 2]>,
    /// Authored mesh coordinates: x right, y up, z forward, before animation.
    pub mesh_position: Option<[f32; 3]>,
}

pub struct InputEventPage<'a> {
    pub events: Vec<&'a VehicleInputEvent>,
    pub next_after: u64,
    pub last_sequence: u64,
    pub missed: bool,
}

#[derive(Default)]
pub(crate) struct InputEvents {
    last_sequence: u64,
    events: VecDeque<VehicleInputEvent>,
}

impl InputEvents {
    pub(crate) fn record(&mut self, mut event: VehicleInputEvent) {
        self.last_sequence = self.last_sequence.checked_add(1)
            .expect("vehicle input sequence exhausted");
        event.sequence = self.last_sequence;
        if self.events.len() == CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub(crate) fn page(&self, after: Option<u64>, limit: usize) -> Result<InputEventPage<'_>, String> {
        if !(1..=64).contains(&limit) {
            return Err("limit must be in 1..64".into());
        }
        let after = after.unwrap_or(self.last_sequence);
        if after > self.last_sequence {
            return Err("after is ahead of this vehicle input sequence".into());
        }
        let missed = self.events.front()
            .is_some_and(|event| after.saturating_add(1) < event.sequence);
        let events: Vec<_> = self.events.iter()
            .filter(|event| event.sequence > after).take(limit).collect();
        Ok(InputEventPage {
            next_after: events.last().map_or(after, |event| event.sequence),
            last_sequence: self.last_sequence,
            missed,
            events,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> VehicleInputEvent {
        VehicleInputEvent { sequence: 0, section: 0, trigger: "panel".into(),
            mesh: "panel.o3d".into(), pressed: true, simulation_seconds: 0.0,
            uv: None, mesh_position: None }
    }

    #[test]
    fn cursors_are_bounded_chronological_and_independent_for_each_reader() {
        let mut ring = InputEvents::default();
        for _ in 0..140 { ring.record(event()); }
        let current = ring.page(None, 64).unwrap();
        assert!(current.events.is_empty());
        assert_eq!(current.next_after, 140);
        assert!(!current.missed);
        let first = ring.page(Some(0), 64).unwrap();
        assert!(first.missed);
        assert_eq!(first.events.first().unwrap().sequence, 13);
        assert_eq!(first.next_after, 76);
        assert_eq!(first.last_sequence, 140);
        let again = ring.page(Some(0), 64).unwrap();
        assert_eq!(first.events, again.events);
        let rest = ring.page(Some(first.next_after), 64).unwrap();
        assert!(!rest.missed);
        assert_eq!(rest.events.first().unwrap().sequence, 77);
        assert_eq!(rest.next_after, 140);
        assert!(ring.page(Some(140), 64).unwrap().events.is_empty());
        assert!(ring.page(Some(141), 64).is_err());
        assert!(ring.page(Some(0), 0).is_err());
        assert!(ring.page(Some(0), 65).is_err());
    }

    #[test]
    fn a_replacement_instance_starts_with_no_old_input_or_cursor() {
        let mut old = InputEvents::default();
        old.record(event());
        let replacement = InputEvents::default();
        assert_eq!(replacement.page(None, 1).unwrap().last_sequence, 0);
        assert!(replacement.page(Some(1), 1).is_err());
        assert_eq!(old.page(Some(0), 1).unwrap().events[0].sequence, 1);
    }
}
