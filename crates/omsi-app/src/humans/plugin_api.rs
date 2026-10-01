//! Native human state access. This intentionally exposes the engine's state machine,
//! not guessed OMSI AIMode enum values or writable synthetic seat pointers.

use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub(super) fn new_epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, AtomicOrdering::Relaxed)
}

pub(super) struct MotionCommand {
    speed: f64,
    heading: f64,
    bus: Option<BusId>,
}

fn walking(state: &State) -> bool {
    matches!(
        state,
        State::Strolling(_)
            | State::ToStop { .. }
            | State::ToSpot { .. }
            | State::Queue { .. }
            | State::Aboard { .. }
            | State::Leaving { .. }
    )
}

pub(super) fn apply_motion(person: &Person, want: &mut Want, command: MotionCommand) {
    let bus = match person.place {
        Place::Ground => None,
        Place::Bus(bus, _) => Some(bus),
    };
    if person.remote || person.puppet.is_some() || !walking(&person.state) || bus != command.bus {
        return;
    }
    let angle = command.heading.to_radians();
    want.vel = DVec2::new(angle.sin(), angle.cos()) * command.speed;
    want.face = Some(command.heading);
    // Keep the goal, aisle corridor and crowd-following rules: this command changes
    // the desired motion, while collision avoidance still decides actual movement.
}

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        // The shared dispatcher validates this envelope field before entering here.
        if key != "session_id" && !allowed.contains(&key.as_str()) {
            return Err(format!("unsupported argument {key}"));
        }
    }
    Ok(())
}

fn parse_handle(value: &Value, epoch: u64) -> Result<u32, String> {
    let mut parts = value
        .as_str()
        .ok_or("id must be a human handle")?
        .split(':');
    let prefix = parts.next();
    let generation = parts.next().and_then(|v| v.parse::<u64>().ok());
    let id = parts.next().and_then(|v| v.parse::<u32>().ok());
    if prefix != Some("human") || generation != Some(epoch) || parts.next().is_some() {
        return Err("invalid or stale human handle".into());
    }
    id.filter(|&v| v != 0)
        .ok_or_else(|| "invalid human id".into())
}

fn number(args: &Value, key: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
    args.get(key)
        .map(|v| {
            v.as_f64()
                .filter(|v| v.is_finite() && *v >= min && *v <= max)
                .ok_or_else(|| format!("{key} must be finite and between {min} and {max}"))
        })
        .transpose()
}

fn bus_id(bus: BusId) -> Value {
    match bus {
        BusId::Player => json!({"kind":"player"}),
        BusId::Ai(id) => json!({"kind":"ai","id":id.to_string()}),
    }
}

fn state(p: &Person) -> Value {
    match &p.state {
        State::Strolling(_) => json!({"kind":"strolling"}),
        State::ToStop { stop, spot, .. } => {
            json!({"kind":"to_stop","stop_id":stop.to_string(),"spot":spot})
        }
        State::ToSpot { stop, spot } => {
            json!({"kind":"to_spot","stop_id":stop.to_string(),"spot":spot})
        }
        State::Waiting {
            stop,
            spot,
            patience,
        } => {
            json!({"kind":"waiting","stop_id":stop.to_string(),"spot":spot,"patience_seconds":patience})
        }
        State::Queue {
            bus,
            entry,
            stop,
            spot,
            joined,
        } => {
            json!({"kind":"queue","bus":bus_id(*bus),"entry":entry,"stop_id":stop.to_string(),"spot":spot,"joined_seconds":joined})
        }
        State::Aboard { bus, goal, .. } => {
            let (name, index) = match goal {
                Goal::Desk(seat) => ("desk", *seat),
                Goal::Stamper(seat, _) => ("stamper", *seat),
                Goal::Seat(seat) => ("seat", *seat),
                Goal::ExitWait(exit) => ("exit_wait", *exit),
                Goal::Exit(exit) => ("exit", *exit),
            };
            json!({"kind":"aboard","bus":bus_id(*bus),"goal":name,"goal_index":index})
        }
        State::AtDesk {
            bus,
            seat,
            ticket,
            done,
        } => json!({"kind":"at_desk","bus":bus_id(*bus),"seat":seat,"ticket":ticket,"done":done}),
        State::Riding { bus, seat } => json!({"kind":"riding","bus":bus_id(*bus),"seat":seat}),
        State::AtExit { bus, exit } => json!({"kind":"at_exit","bus":bus_id(*bus),"exit":exit}),
        State::Leaving { walked, .. } => json!({"kind":"leaving","walked_metres":walked}),
    }
}

impl Humans {
    fn plugin_person(&self, p: &Person) -> Value {
        let place = match p.place {
            Place::Ground => json!({"kind":"ground"}),
            Place::Bus(bus, position) => json!({"kind":"bus","bus":bus_id(bus),
                "local_position":{"right":position.x,"forward":position.y,"up":position.z}}),
        };
        let desired = self
            .plugin_desired_motion
            .get(&p.id)
            .map(|(velocity, heading)| {
                json!({
            "speed_metres_per_second":velocity.length(),"heading_degrees":heading,
            "velocity":{"right_or_east":velocity.x,"forward_or_north":velocity.y}})
            });
        json!({"id":format!("human:{}:{}",self.plugin_epoch,p.id),"native_id":p.id,
            "file_name":p.ty.def.path.to_string_lossy(),"state":state(p),"state_seconds":p.t_state,
            "definition":{"height_metres":p.ty.def.height,"mass_kg":p.ty.def.mass,
                "seat_height_metres":p.ty.def.seat_height,"walk_parameters":p.ty.def.walk_param},
            "age_years":p.age,"activity":format!("{:?}",p.activity).to_ascii_lowercase(),
            "position":{"east":p.position.x,"north":p.position.y,"up":p.position.z},
            "heading_degrees":p.heading,"local_heading_degrees":p.lheading,"place":place,
            "velocity":{"right_or_east":p.vel.x,"forward_or_north":p.vel.y},
            "speed_metres_per_second":p.vel.length(),"pace_metres_per_second":p.pace,
            "desired_motion":desired,"motion_command_pending":self.plugin_motion.contains_key(&p.id),
            "exit_stop":p.exit_stop,"exit_stop_id":p.exit_id.map(|v|v.to_string()),
            "boarded_at_stop_id":p.from.to_string(),"stops_left":p.stops_left,
            "leaving_here":p.leaving_here,"ticket_index":p.ticket,"ticket_decided":p.ticket_decided,
            "stamps_ticket":p.stamps,"pause_until_seconds":p.pause_until,
            "blocked_seconds":p.blocked,"stuck_seconds":p.stuck,"waiting_reason":p.why,
            "target_preference_fraction":p.target,
            "remote":p.remote,"avatar":self.avatars.values().any(|id|*id==p.id)})
    }

    pub(crate) fn plugin_api(
        &mut self,
        operation: &str,
        args: &Value,
        player_stop_ids: Option<&[i64]>,
    ) -> Result<Value, String> {
        if operation == "humans.fares.list" {
            return self.plugin_fares(args);
        }
        if operation == "humans.list" {
            keys(args, &["after_id", "limit"])?;
            let after = args
                .get("after_id")
                .map(|v| parse_handle(v, self.plugin_epoch))
                .transpose()?
                .unwrap_or(0);
            let limit = args
                .get("limit")
                .map(|v| {
                    v.as_u64()
                        .filter(|n| *n >= 1 && *n <= 512)
                        .ok_or("limit must be between 1 and 512")
                })
                .transpose()?
                .unwrap_or(128) as usize;
            let mut people: Vec<_> = self.people.iter().filter(|p| p.id > after).collect();
            people.sort_by_key(|p| p.id);
            let more = people.len() > limit;
            people.truncate(limit);
            let next = if more {
                people
                    .last()
                    .map(|p| format!("human:{}:{}", self.plugin_epoch, p.id))
            } else {
                None
            };
            return Ok(
                json!({"generation":self.plugin_epoch.to_string(),"items":people.iter().map(|p|self.plugin_person(p)).collect::<Vec<_>>(),"next_after_id":next}),
            );
        }
        let id = parse_handle(args.get("id").ok_or("id is required")?, self.plugin_epoch)?;
        let index = self
            .people
            .iter()
            .position(|p| p.id == id)
            .ok_or("human is no longer loaded")?;
        if operation == "humans.ticket.get" {
            keys(args, &["id"])?;
            return Ok(self.plugin_ticket(index));
        }
        if operation == "humans.ticket.set" {
            return self.plugin_ticket_set(index, args);
        }
        match operation {
            "humans.get" => {
                keys(args, &["id"])?;
            }
            "humans.set" => self.plugin_set(index, args, player_stop_ids)?,
            "humans.reassign_entry" => self.plugin_reassign_entry(index, args)?,
            "humans.assign_seat" => self.plugin_assign_seat(index, args)?,
            "humans.control" => self.plugin_control(index, args)?,
            _ => return Err(format!("unknown native human operation {operation}")),
        }
        Ok(self.plugin_person(&self.people[index]))
    }

    fn plugin_fares(&self, args: &Value) -> Result<Value, String> {
        keys(args, &["offset", "limit"])?;
        let offset = args
            .get("offset")
            .map(|v| {
                v.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("offset must be a nonnegative integer")
            })
            .transpose()?
            .unwrap_or(0);
        let limit = args
            .get("limit")
            .map(|v| {
                v.as_u64()
                    .filter(|n| (1..=256).contains(n))
                    .ok_or("limit must be in 1..256")
            })
            .transpose()?
            .unwrap_or(64) as usize;
        let tickets = self
            .tickets
            .as_ref()
            .map(|p| p.tickets.as_slice())
            .unwrap_or_default();
        if offset > tickets.len() {
            return Err("offset is beyond the active ticket catalogue".into());
        }
        let end = offset.saturating_add(limit).min(tickets.len());
        let items:Vec<_>=tickets[offset..end].iter().enumerate().map(|(i,t)|json!({
            "index":offset+i,"name":t.name,"name_english":t.name_english,"value":t.value,
            "display_string":t.display_string,"max_stations":t.max_stations,
            "age_min":t.age_min,"age_max":t.age_max,"day_ticket":t.day_ticket,"probability":t.probability})).collect();
        Ok(
            json!({"source_file":self.tickets.as_ref().map(|p|p.path.to_string_lossy()),
            "items":items,"total":tickets.len(),"next_offset":if end<tickets.len(){Some(end)}else{None},
            "selection":self.tickets.as_ref().map(|p|json!({"stamper_probability":p.stamper_prop,
                "purchase_probability":p.ticketbuy_prop,"chattiness":p.chattiness,"complaint_probability":p.whinge_prop}))}),
        )
    }

    fn plugin_ticket(&self, index: usize) -> Value {
        let p = &self.people[index];
        let mode = if !p.ticket_decided {
            "auto"
        } else if p.ticket.is_some() {
            "buy"
        } else if p.stamps {
            "stamp"
        } else {
            "pass"
        };
        let product = p.ticket.and_then(|i| {
            self.tickets.as_ref()?.tickets.get(i).map(|t|
            json!({"index":i,"name":t.name,"value":t.value,"display_string":t.display_string}))
        });
        // Cash-desk state is shared by the actual current player transaction. It must
        // never appear as every waiting passenger's personal payment.
        let transaction = match p.state {
            State::AtDesk {
                bus: BusId::Player,
                ticket,
                done,
                ..
            } => Some(json!({
                "ticket_index":ticket,"ticket_issued":done,
                "requested_name":self.request.as_ref().map(|r|&r.0),
                "requested_value":self.request.as_ref().map(|r|r.1),
                "paid_amount":self.paid.map(|paid|paid.0),"payment_price":self.paid.map(|paid|paid.1),
                "change_due":self.change_due,"change_on_tray":self.money.as_ref().map(|m|m.change_value())})),
            _ => None,
        };
        json!({"id":format!("human:{}:{}",self.plugin_epoch,p.id),"mode":mode,
            "ticket_index":p.ticket,"decided":p.ticket_decided,"product":product,"transaction":transaction})
    }

    fn plugin_ticket_set(&mut self, index: usize, args: &Value) -> Result<Value, String> {
        keys(args, &["id", "mode", "ticket_index"])?;
        self.plugin_writable(index)?;
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .ok_or("mode must be buy, pass, stamp or auto")?;
        if !["buy", "pass", "stamp", "auto"].contains(&mode) {
            return Err("mode must be buy, pass, stamp or auto".into());
        }
        let p = &self.people[index];
        if p.place != Place::Ground
            || !matches!(
                p.state,
                State::ToStop { .. }
                    | State::ToSpot { .. }
                    | State::Waiting { .. }
                    | State::Queue { .. }
            )
        {
            return Err("ticket choice can change only before boarding; an active payment or cabin route cannot be rewritten".into());
        }
        let ticket = if mode == "buy" {
            let i = args
                .get("ticket_index")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("buy requires a nonnegative ticket_index from humans.fares.list")?;
            let t = self
                .tickets
                .as_ref()
                .and_then(|pack| pack.tickets.get(i))
                .ok_or("ticket_index is not in the active ticket catalogue")?;
            if !t.value.is_finite() || t.value < 0.0 {
                return Err("the selected product has an invalid fare".into());
            }
            if p.age < t.age_min as f32 || p.age > t.age_max as f32 {
                return Err("the selected product is outside the passenger's age range".into());
            }
            Some(i)
        } else {
            if args.get("ticket_index").is_some() {
                return Err("ticket_index is allowed only for buy mode".into());
            }
            None
        };
        if let State::Queue { bus, entry, .. } = p.state {
            if mode == "auto" {
                return Err("an already queued passenger needs an explicit ticket choice".into());
            }
            let bn = self
                .last_buses
                .iter()
                .find(|b| b.id == bus)
                .ok_or("the queued bus is no longer loaded")?;
            let door = bn
                .cabin
                .entries
                .get(entry)
                .ok_or("the queued entry is unavailable")?;
            if ticket.is_some() && (!door.sells || bn.cabin.desk.is_none()) {
                return Err("the queued entry cannot sell tickets; select a selling entry before changing the fare".into());
            }
            if mode == "stamp" && bn.cabin.stampers.is_empty() {
                return Err("the queued bus has no usable validator".into());
            }
        }
        // These are the fields consumed by decide_ticket, choose_entry and the real
        // desk/validator routing. No passenger, seat or platform reservation is moved.
        let p = &mut self.people[index];
        p.ticket = ticket;
        p.stamps = mode == "stamp";
        p.ticket_decided = mode != "auto";
        Ok(
            json!({"id":format!("human:{}:{}",self.plugin_epoch,p.id),"mode":mode,
            "ticket_index":p.ticket,"decided":p.ticket_decided}),
        )
    }

    fn plugin_writable(&self, index: usize) -> Result<(), String> {
        let p = &self.people[index];
        if p.remote || p.puppet.is_some() || self.avatars.values().any(|id| *id == p.id) {
            return Err("remote people, test puppets and player avatars are not writable".into());
        }
        Ok(())
    }

    fn plugin_set(
        &mut self,
        index: usize,
        args: &Value,
        stop_ids: Option<&[i64]>,
    ) -> Result<(), String> {
        keys(
            args,
            &[
                "id",
                "pace_metres_per_second",
                "speed_metres_per_second",
                "exit_stop",
                "heading_degrees",
                "state_seconds",
                "waiting_patience_seconds",
            ],
        )?;
        self.plugin_writable(index)?;
        let pace = number(args, "pace_metres_per_second", 0.1, 5.0)?;
        let speed = number(args, "speed_metres_per_second", 0.0, 5.0)?;
        let heading = number(args, "heading_degrees", -360000.0, 360000.0)?;
        let elapsed = number(args, "state_seconds", 0.0, 86400.0)?;
        let patience = number(args, "waiting_patience_seconds", 0.0, 86400.0)?;
        let exit = args
            .get("exit_stop")
            .map(|v| {
                v.as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or("exit_stop must be an integer")
            })
            .transpose()?;
        let p = &self.people[index];
        if patience.is_some() && !matches!(p.state, State::Waiting { .. }) {
            return Err("waiting_patience_seconds requires a waiting passenger".into());
        }
        if speed.is_some() && !walking(&p.state) {
            return Err("instantaneous speed is writable only while walking".into());
        }
        let exit_id = if let Some(exit) = exit {
            if !matches!(p.place, Place::Bus(BusId::Player, _))
                || !matches!(
                    p.state,
                    State::Riding {
                        bus: BusId::Player,
                        ..
                    } | State::AtDesk {
                        bus: BusId::Player,
                        ..
                    } | State::Aboard {
                        bus: BusId::Player,
                        goal: Goal::Seat(_) | Goal::Desk(_) | Goal::Stamper(..),
                        ..
                    }
                )
            {
                return Err("exit_stop is writable only for a rider aboard the player's bus before alighting".into());
            }
            if exit == -1 {
                None
            } else {
                let stops = stop_ids.ok_or("the player has no current timetable stop list")?;
                let id = usize::try_from(exit)
                    .ok()
                    .and_then(|i| stops.get(i))
                    .copied()
                    .filter(|id| *id != 0)
                    .ok_or("exit_stop does not identify a current timetable stop")?;
                Some(id)
            }
        } else {
            None
        };
        if pace.is_none()
            && speed.is_none()
            && exit.is_none()
            && heading.is_none()
            && elapsed.is_none()
            && patience.is_none()
        {
            return Err("no human fields were supplied".into());
        }
        let local_heading = heading
            .map(|heading| match p.place {
                Place::Ground => Ok(heading.rem_euclid(360.0)),
                Place::Bus(bus, local) => self
                    .last_buses
                    .iter()
                    .find(|b| b.id == bus)
                    .map(|b| (heading - b.heading_at(local)).rem_euclid(360.0))
                    .ok_or("the person's bus pose is not available"),
            })
            .transpose()?;
        // All validation precedes mutation so a rejected batch leaves the person untouched.
        let p = &mut self.people[index];
        if let Some(value) = pace {
            p.pace = value;
        }
        if let Some(value) = heading {
            p.heading = value.rem_euclid(360.0);
        }
        if let Some(value) = local_heading {
            p.lheading = value;
        }
        if let Some(value) = elapsed {
            p.t_state = value as f32;
        }
        if let (Some(value), State::Waiting { patience, .. }) = (patience, &mut p.state) {
            *patience = value as f32;
        }
        if let Some(value) = speed {
            let h = if p.place == Place::Ground {
                p.heading
            } else {
                p.lheading
            }
            .to_radians();
            let direction = if p.vel.length_squared() > 1e-12 {
                p.vel.normalize()
            } else {
                DVec2::new(h.sin(), h.cos())
            };
            p.vel = direction * value;
        }
        if let Some(value) = exit {
            p.exit_stop = value;
            p.exit_id = exit_id;
            p.leaving_here = exit_id.is_some() && exit_id == self.served_stop;
            self.requested_for = None;
        }
        Ok(())
    }

    fn plugin_control(&mut self, index: usize, args: &Value) -> Result<(), String> {
        keys(args, &["id", "speed_metres_per_second", "heading_degrees"])?;
        self.plugin_writable(index)?;
        let speed = number(args, "speed_metres_per_second", 0.0, 5.0)?
            .ok_or("speed_metres_per_second is required")?;
        let heading = number(args, "heading_degrees", -360000.0, 360000.0)?
            .ok_or("heading_degrees is required")?;
        let p = &self.people[index];
        if !walking(&p.state) {
            return Err("desired motion can only control a walking person".into());
        }
        let bus = match p.place {
            Place::Ground => None,
            Place::Bus(bus, _) => Some(bus),
        };
        self.plugin_motion.insert(
            p.id,
            MotionCommand {
                speed,
                heading: heading.rem_euclid(360.0),
                bus,
            },
        );
        Ok(())
    }

    fn plugin_assign_seat(&mut self, index: usize, args: &Value) -> Result<(), String> {
        keys(args, &["id", "seat"])?;
        self.plugin_writable(index)?;
        let seat = args
            .get("seat")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("seat must be a non-negative passenger place index")?;
        let p = &self.people[index];
        let State::Riding { bus, seat: old } = p.state else {
            return Err("seat reassignment requires a riding passenger".into());
        };
        let Place::Bus(place_bus, local) = p.place else {
            return Err("a rider must be inside its bus".into());
        };
        if place_bus != bus || p.leaving_here {
            return Err("the passenger is changing buses or preparing to alight".into());
        }
        let bn = self
            .last_buses
            .iter()
            .find(|b| b.id == bus)
            .ok_or("the passenger's bus is no longer loaded")?;
        let current = bn
            .cabin
            .seats
            .get(old)
            .ok_or("the current passenger place is not in this cabin")?;
        let target = bn
            .cabin
            .seats
            .get(seat)
            .ok_or("the requested passenger place is not in this cabin")?;
        let taken = self
            .seats
            .get(&bus)
            .ok_or("the bus has no live seat reservation table")?;
        if taken.len() != bn.cabin.seats.len() || !taken.get(old).copied().unwrap_or(false) {
            return Err("the passenger's current place is not reserved".into());
        }
        if seat == old {
            return Ok(());
        }
        if taken[seat] {
            return Err("the requested passenger place is already reserved".into());
        }
        let claimed = |p: &Person| match p.state {
            State::Riding { bus: b, seat: s }
            | State::AtDesk {
                bus: b, seat: s, ..
            }
            | State::Aboard {
                bus: b,
                goal: Goal::Seat(s) | Goal::Desk(s) | Goal::Stamper(s, _),
                ..
            } => b == bus && (s == seat || s == old),
            _ => false,
        };
        if self
            .people
            .iter()
            .enumerate()
            .any(|(other, p)| other != index && claimed(p))
        {
            return Err(
                "the passenger place reservation is inconsistent with another rider".into(),
            );
        }
        let start = if current.seated {
            Vec3::new(local.x, local.y, current.floor.z)
        } else {
            local
        };
        let mut route = vec![current.floor];
        let continuation = bn.cabin.route(current.floor, target.floor);
        if continuation.is_empty() {
            return Err("the cabin has no walking route to the requested place".into());
        }
        route.extend(continuation);
        // Reserve the new place before the rider starts moving. There is no interval
        // in which another boarding passenger can choose it or the old seat is leaked.
        let taken = self.seats.get_mut(&bus).unwrap();
        taken[seat] = true;
        taken[old] = false;
        self.people[index].place = Place::Bus(bus, start);
        self.people[index].vel = DVec2::ZERO;
        self.people[index].activity = Activity::Stand;
        self.set_state(
            index,
            State::Aboard {
                bus,
                route,
                idx: 0,
                seg: start,
                goal: Goal::Seat(seat),
            },
        );
        Ok(())
    }

    fn plugin_reassign_entry(&mut self, index: usize, args: &Value) -> Result<(), String> {
        keys(args, &["id", "entry"])?;
        self.plugin_writable(index)?;
        let entry = args
            .get("entry")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("entry must be a non-negative integer")?;
        let p = &self.people[index];
        let State::Queue {
            bus,
            entry: old_entry,
            stop,
            spot,
            joined,
        } = p.state
        else {
            return Err("only a queued passenger can be assigned another entry".into());
        };
        if p.place != Place::Ground {
            return Err("a queued passenger must still be on the ground".into());
        }
        let bn = self
            .last_buses
            .iter()
            .find(|b| b.id == bus)
            .ok_or("the passenger's bus is no longer loaded")?;
        if !bn.standing() || bn.stop != Some(stop) {
            return Err("the bus must be standing at this passenger's stop".into());
        }
        let door = bn
            .cabin
            .entries
            .get(entry)
            .ok_or("entry is not part of this bus cabin")?;
        if !bn.entry_open.get(entry).copied().unwrap_or(false) {
            return Err("the requested entry is not open".into());
        }
        if p.ticket.is_some() && !door.sells {
            return Err("this passenger must use an entry with ticket sales".into());
        }
        let taken = self
            .seats
            .get(&bus)
            .ok_or("the bus has no live seat reservation table")?;
        if taken.len() != bn.cabin.seats.len() || taken.iter().all(|v| *v) {
            return Err("the bus has no available passenger place".into());
        }
        if self
            .stops
            .get(&stop)
            .and_then(|s| s.spots.get(spot))
            .and_then(|s| s.taken)
            != Some(p.id)
        {
            return Err("the passenger no longer owns its waiting place".into());
        }
        if entry == old_entry {
            return Ok(());
        }
        // Queue members have not reserved seats yet. Preserve their waiting spot and
        // leave all seat/doorway reservations intact; the normal boarding tick owns them.
        self.set_state(
            index,
            State::Queue {
                bus,
                entry,
                stop,
                spot,
                joined: joined.max(self.time),
            },
        );
        self.people[index].vel = DVec2::ZERO;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Humans {
        let folder = std::env::temp_dir().join(format!(
            "openomsi-human-api-{}-{}",
            std::process::id(),
            new_epoch()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let human_path = folder.join("fixture.hum");
        let model_path = folder.join("model.cfg");
        std::fs::write(
            &human_path,
            "[model]\nmodel.cfg\n[humangeom]\n0.2\n1.7\n[seatheight]\n0.8\n",
        )
        .unwrap();
        std::fs::write(&model_path, "").unwrap();
        let ty = Arc::new(HumanType::load(&human_path).unwrap());
        let mut h = Humans::new(&folder);
        std::fs::remove_file(&human_path).unwrap();
        std::fs::remove_file(&model_path).unwrap();
        std::fs::remove_dir(&folder).unwrap();
        h.people.push(Person {
            id: 1,
            ty,
            variant: 0,
            meshes: vec![],
            position: DVec3::ZERO,
            heading: 0.0,
            lheading: 0.0,
            place: Place::Ground,
            vel: DVec2::ZERO,
            pace: 1.4,
            activity: Activity::Stand,
            anim: Pose::new(1),
            state: State::Waiting {
                stop: 7,
                spot: 0,
                patience: 120.0,
            },
            t_state: 0.0,
            skins: vec![],
            interior: 0.0,
            lit: 0.0,
            tilt: Mat4::IDENTITY,
            from: -1,
            exit_stop: -1,
            stops_left: 1,
            exit_id: None,
            leaving_here: false,
            avoid: None,
            target: 0.5,
            ticket: None,
            ticket_decided: true,
            stamps: false,
            pause_until: 0.0,
            age: 40.0,
            stuck: 0.0,
            ghost: 0.0,
            car_wait: 0.0,
            detour: 0.0,
            detour_side: 0.0,
            blocked: 0.0,
            why: "",
            why_logged: "",
            skinned: false,
            since_posed: 0,
            posed_at: (DVec3::ZERO, 0.0),
            ankles: [Vec3::ZERO; 2],
            puppet: None,
            remote: false,
        });
        let door = Door {
            inside: Vec3::ZERO,
            outside: Vec3::ZERO,
            side: 1.0,
            queue_dir: 1.0,
            sells: true,
            wait: Vec3::ZERO,
            aisle: Vec3::ZERO,
        };
        let cabin = Arc::new(Cabin {
            data: PassengerCabin::default(),
            graph: PathGraph::default(),
            links: vec![],
            link_pack: vec![],
            step_packs: vec![],
            entries: vec![door.clone(), door],
            exits: vec![],
            desk: None,
            stampers: vec![],
            seats: vec![
                Seat {
                    pos: Vec3::new(0.0, 0.0, 0.8),
                    floor: Vec3::ZERO,
                    rot: 0.0,
                    seated: true,
                },
                Seat {
                    pos: Vec3::new(0.0, 2.0, 0.8),
                    floor: Vec3::new(0.0, 2.0, 0.0),
                    rot: 0.0,
                    seated: true,
                },
            ],
            parts: vec![],
        });
        h.last_buses.push(BusNow {
            id: BusId::Player,
            cabin,
            pos: DVec3::ZERO,
            rot: Mat4::IDENTITY,
            heading: 0.0,
            speed: 0.0,
            entry_open: vec![true, true],
            exit_open: vec![],
            walk_open: None,
            stop: Some(7),
            approach: None,
            interior: 0.0,
            air: CabinAir::default(),
            half: DVec2::ONE,
            centre: DVec2::ZERO,
            accel: DVec2::ZERO,
            trailers: vec![],
            terminus: Some("Test".into()),
        });
        h.seats.insert(BusId::Player, vec![false, false]);
        h.stops.insert(
            7,
            StopInfo {
                name: "Test".into(),
                pos: DVec3::ZERO,
                spots: vec![Spot {
                    pos: DVec3::ZERO,
                    face: 0.0,
                    seat: 0.0,
                    taken: Some(1),
                }],
                lane: None,
                seeded: true,
                next_arrival: 100.0,
            },
        );
        h
    }

    fn identity(h: &Humans) -> String {
        format!("human:{}:1", h.plugin_epoch)
    }

    #[test]
    fn native_human_writes_validate_entire_batch_before_mutating() {
        let mut h = fixture();
        let id = identity(&h);
        let before = h.plugin_api("humans.get", &json!({"id":id}), None).unwrap();
        assert!(h
            .plugin_api(
                "humans.set",
                &json!({"id":id,"pace_metres_per_second":2.0,"exit_stop":99}),
                Some(&[7, 8])
            )
            .is_err());
        assert_eq!(
            h.plugin_api("humans.get", &json!({"id":id}), None).unwrap(),
            before
        );
        h.plugin_api(
            "humans.set",
            &json!({"id":id,"pace_metres_per_second":2.0,"waiting_patience_seconds":30.0}),
            None,
        )
        .unwrap();
        assert_eq!(h.people[0].pace, 2.0);
        assert!(matches!(
            h.people[0].state,
            State::Waiting { patience: 30.0, .. }
        ));
        h.people[0].remote = true;
        assert!(h
            .plugin_api(
                "humans.set",
                &json!({"id":id,"pace_metres_per_second":1.0}),
                None
            )
            .is_err());
        assert_eq!(h.people[0].pace, 2.0);
    }

    #[test]
    fn native_queue_reassignment_preserves_reservations_and_rejects_closed_doors() {
        let mut h = fixture();
        let id = identity(&h);
        h.people[0].state = State::Queue {
            bus: BusId::Player,
            entry: 0,
            stop: 7,
            spot: 0,
            joined: 1.0,
        };
        h.last_buses[0].entry_open[1] = false;
        assert!(h
            .plugin_api("humans.reassign_entry", &json!({"id":id,"entry":1}), None)
            .is_err());
        assert!(matches!(h.people[0].state, State::Queue { entry: 0, .. }));
        h.last_buses[0].entry_open[1] = true;
        h.plugin_api("humans.reassign_entry", &json!({"id":id,"entry":1}), None)
            .unwrap();
        assert!(matches!(h.people[0].state, State::Queue { entry: 1, .. }));
        assert_eq!(h.seats[&BusId::Player], vec![false, false]);
        assert_eq!(h.stops[&7].spots[0].taken, Some(1));
    }

    #[test]
    fn native_seat_assignment_moves_reservation_and_walks_instead_of_teleporting() {
        let mut h = fixture();
        let id = identity(&h);
        h.people[0].place = Place::Bus(BusId::Player, Vec3::new(0.0, 0.0, 0.8));
        h.people[0].state = State::Riding {
            bus: BusId::Player,
            seat: 0,
        };
        h.seats.insert(BusId::Player, vec![true, true]);
        assert!(h
            .plugin_api("humans.assign_seat", &json!({"id":id,"seat":1}), None)
            .is_err());
        assert!(matches!(h.people[0].state, State::Riding { seat: 0, .. }));
        h.seats.get_mut(&BusId::Player).unwrap()[1] = false;
        h.plugin_api("humans.assign_seat", &json!({"id":id,"seat":1}), None)
            .unwrap();
        assert_eq!(h.seats[&BusId::Player], vec![false, true]);
        assert_eq!(h.people[0].place, Place::Bus(BusId::Player, Vec3::ZERO));
        assert!(matches!(
            h.people[0].state,
            State::Aboard {
                goal: Goal::Seat(1),
                ..
            }
        ));
    }

    #[test]
    fn native_exit_assignment_keeps_stop_identity_and_current_stop_request_consistent() {
        let mut h = fixture();
        let id = identity(&h);
        h.people[0].place = Place::Bus(BusId::Player, Vec3::ZERO);
        h.people[0].state = State::Riding {
            bus: BusId::Player,
            seat: 0,
        };
        h.served_stop = Some(8);
        h.requested_for = Some(0);
        h.plugin_api("humans.set", &json!({"id":id,"exit_stop":1}), Some(&[7, 8]))
            .unwrap();
        assert_eq!(h.people[0].exit_stop, 1);
        assert_eq!(h.people[0].exit_id, Some(8));
        assert!(h.people[0].leaving_here);
        assert_eq!(h.requested_for, None);
        h.people[0].leaving_here = false;
        h.plugin_api(
            "humans.set",
            &json!({"id":id,"exit_stop":-1}),
            Some(&[7, 8]),
        )
        .unwrap();
        assert_eq!(h.people[0].exit_id, None);
        assert_eq!(h.people[0].exit_stop, -1);
    }

    #[test]
    fn native_motion_control_enters_real_want_pipeline_without_overriding_crowd_rules() {
        let mut h = fixture();
        let id = identity(&h);
        h.people[0].state = State::Queue {
            bus: BusId::Player,
            entry: 0,
            stop: 7,
            spot: 0,
            joined: 0.0,
        };
        h.plugin_api(
            "humans.control",
            &json!({"id":id,"speed_metres_per_second":2.0,"heading_degrees":90.0}),
            None,
        )
        .unwrap();
        assert_eq!(h.people[0].vel, DVec2::ZERO);
        let mut pending = std::mem::take(&mut h.plugin_motion);
        let mut want = Want::stand(None, Activity::Stand);
        want.follow = true;
        want.goal_dist = Some(8.0);
        let corridor = (DVec2::ZERO, DVec2::ONE, 0.5);
        want.corridor = Some(corridor);
        apply_motion(&h.people[0], &mut want, pending.remove(&1).unwrap());
        assert!((want.vel - DVec2::new(2.0, 0.0)).length() < 1e-9);
        assert_eq!(want.face, Some(90.0));
        assert!(want.follow);
        assert_eq!(want.corridor, Some(corridor));
        assert_eq!(want.goal_dist, Some(8.0));
        assert!(h.plugin_motion.is_empty());
        h.people[0].state = State::Riding {
            bus: BusId::Player,
            seat: 0,
        };
        let mut idle = Want::stand(None, Activity::Sit);
        apply_motion(
            &h.people[0],
            &mut idle,
            MotionCommand {
                speed: 2.0,
                heading: 90.0,
                bus: None,
            },
        );
        assert_eq!(idle.vel, DVec2::ZERO);
    }
    #[test]
    fn native_network_reset_retires_old_ground_routes_and_preserves_cabin_ownership() {
        let mut h = fixture();
        h.people[0].state = State::ToStop {
            stop: 7,
            spot: 0,
            walk: PedWalk::new(
                vec![Leg {
                    lane: usize::MAX,
                    a: 0.0,
                    b: 10.0,
                }],
                false,
                0.0,
            ),
        };
        let old_handle = identity(&h);
        let mut rider = fixture().people.pop().unwrap();
        rider.id = 2;
        rider.place = Place::Bus(BusId::Player, Vec3::ZERO);
        rider.state = State::AtDesk {
            bus: BusId::Player,
            seat: 0,
            ticket: Some(0),
            done: false,
        };
        rider.exit_stop = 1;
        rider.exit_id = Some(8);
        let mut avatar = fixture().people.pop().unwrap();
        avatar.id = 3;
        avatar.state = State::Strolling(PedWalk::new(
            vec![Leg {
                lane: usize::MAX,
                a: 0.0,
                b: 10.0,
            }],
            true,
            0.0,
        ));
        h.avatars.insert(0, 3);
        h.people.push(rider);
        h.people.push(avatar);
        h.seats.insert(BusId::Player, vec![true, false]);
        h.paid = Some((5.0, 3.0));
        h.change_due = Some(2.0);
        h.request = Some(("Ticket".into(), 3.0));
        h.ped = Some(PedNet::build(&Network::default()));
        h.plugin_desired_motion.insert(1, (DVec2::ONE, Some(90.0)));
        assert_eq!(h.invalidate_traffic_network(), 1);
        assert!(h.ped.is_none());
        assert!(h.stops.is_empty());
        assert!(h.plugin_desired_motion.is_empty());
        assert!(h
            .plugin_api("humans.get", &json!({"id":old_handle}), None)
            .is_err());
        assert_eq!(h.people.len(), 2);
        assert_eq!(h.seats[&BusId::Player], vec![true, false]);
        assert_eq!(h.paid, Some((5.0, 3.0)));
        assert_eq!(h.change_due, Some(2.0));
        assert!(h.request.is_some());
        assert!(matches!(
            h.people.iter().find(|p| p.id == 2).unwrap().state,
            State::AtDesk { seat: 0, .. }
        ));
        assert!(matches!(
            h.people.iter().find(|p| p.id == 3).unwrap().state,
            State::Leaving { walk: None, .. }
        ));
        h.reconcile_player_stop_ids(&[8, 7]);
        assert_eq!(h.people.iter().find(|p| p.id == 2).unwrap().exit_stop, 0);
        h.reconcile_player_stop_ids(&[7]);
        let rider = h.people.iter().find(|p| p.id == 2).unwrap();
        assert_eq!(rider.exit_stop, -1);
        assert_eq!(rider.exit_id, None);
    }
    #[test]
    fn native_ticket_catalogue_transition_preserves_product_and_rejects_pending_payment() {
        use omsi_content::tickets::{Ticket, TicketPack};
        let mut h = fixture();
        let adult = Ticket {
            name: "Adult".into(),
            age_min: 0,
            age_max: 120,
            value: 2.0,
            probability: 1.0,
            ..Default::default()
        };
        let child = Ticket {
            name: "Child".into(),
            age_min: 0,
            age_max: 12,
            value: 1.0,
            probability: 1.0,
            ..Default::default()
        };
        let old = Arc::new(TicketPack {
            tickets: vec![adult.clone(), child.clone()],
            ..Default::default()
        });
        let next = Some(Arc::new(TicketPack {
            tickets: vec![
                child,
                Ticket {
                    value: 3.0,
                    ..adult
                },
            ],
            ..Default::default()
        }));
        h.tickets = Some(old.clone());
        h.people[0].ticket = Some(0);
        h.paid = Some((2.0, 2.0));
        assert!(h.preflight_ticket_pack(&next).is_err());
        assert_eq!(h.tickets.as_ref().unwrap().tickets[0].value, 2.0);
        assert_eq!(h.preflight_ticket_pack(&Some(old)).unwrap(), false);
        h.paid = None;
        h.people[0].state = State::AtDesk {
            bus: BusId::Player,
            seat: 0,
            ticket: Some(0),
            done: false,
        };
        assert!(h.preflight_ticket_pack(&next).is_err());
        h.people[0].state = State::Aboard {
            bus: BusId::Player,
            route: vec![],
            idx: 0,
            seg: Vec3::ZERO,
            goal: Goal::Desk(0),
        };
        assert!(h.preflight_ticket_pack(&next).unwrap());
        h.commit_ticket_pack(next);
        assert_eq!(h.people[0].ticket, Some(1));
        assert_eq!(h.tickets.as_ref().unwrap().tickets[1].value, 3.0);
        assert!(h.preflight_ticket_pack(&None).unwrap());
        h.commit_ticket_pack(None);
        assert_eq!(h.people[0].ticket, None);
    }
    #[test]
    fn native_ticket_choice_changes_actual_entry_selection_and_preserves_payment() {
        use omsi_content::tickets::{Ticket, TicketPack};
        let mut h = fixture();
        let id = identity(&h);
        h.boarding = "pay".into();
        h.tickets = Some(Arc::new(TicketPack {
            ticketbuy_prop: 1.0,
            tickets: vec![
                Ticket {
                    name: "Adult".into(),
                    value: 2.5,
                    age_min: 18,
                    age_max: 120,
                    probability: 1.0,
                    ..Default::default()
                },
                Ticket {
                    name: "Child".into(),
                    value: 1.0,
                    age_min: 0,
                    age_max: 17,
                    probability: 1.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }));
        let cabin = Arc::get_mut(&mut h.last_buses[0].cabin).unwrap();
        cabin.entries[0].sells = false;
        cabin.entries[1].outside = Vec3::new(4.0, 0.0, 0.0);
        assert_eq!(h.choose_entry(0, &h.last_buses[0]), Some(0));
        h.plugin_api(
            "humans.ticket.set",
            &json!({"id":id,"mode":"buy","ticket_index":0}),
            None,
        )
        .unwrap();
        assert_eq!(
            h.choose_entry(0, &h.last_buses[0]),
            Some(1),
            "actual boarding must now choose the selling entry"
        );
        let before = h.plugin_ticket(0);
        assert!(h
            .plugin_api(
                "humans.ticket.set",
                &json!({"id":id,"mode":"buy","ticket_index":1}),
                None
            )
            .is_err());
        assert_eq!(
            h.plugin_ticket(0),
            before,
            "an age-invalid product must leave the prior choice untouched"
        );
        h.plugin_api("humans.ticket.set", &json!({"id":id,"mode":"pass"}), None)
            .unwrap();
        assert_eq!(h.choose_entry(0, &h.last_buses[0]), Some(0));
        h.people[0].state = State::Queue {
            bus: BusId::Player,
            entry: 0,
            stop: 7,
            spot: 0,
            joined: 0.0,
        };
        assert!(h
            .plugin_api(
                "humans.ticket.set",
                &json!({"id":id,"mode":"buy","ticket_index":0}),
                None
            )
            .is_err());
        assert_eq!(h.people[0].ticket, None);
        assert_eq!(h.stops[&7].spots[0].taken, Some(1));
        h.people[0].state = State::Waiting {
            stop: 7,
            spot: 0,
            patience: 30.0,
        };
        h.plugin_api("humans.ticket.set", &json!({"id":id,"mode":"auto"}), None)
            .unwrap();
        assert!(!h.people[0].ticket_decided);
        h.decide_ticket(0, false, true);
        assert_eq!(
            h.people[0].ticket,
            Some(0),
            "auto must return to the real age-weighted ticket decision"
        );
        h.paid = Some((5.0, 2.5));
        h.request = Some(("Adult".into(), 2.5));
        assert_eq!(
            h.plugin_ticket(0)["transaction"],
            Value::Null,
            "cash cannot be attributed to an unrelated waiting person"
        );
        h.people[0].place = Place::Bus(BusId::Player, Vec3::ZERO);
        h.people[0].state = State::AtDesk {
            bus: BusId::Player,
            seat: 0,
            ticket: Some(0),
            done: false,
        };
        assert_eq!(h.plugin_ticket(0)["transaction"]["paid_amount"], 5.0);
        assert!(h
            .plugin_api("humans.ticket.set", &json!({"id":id,"mode":"pass"}), None)
            .is_err());
        assert_eq!(h.paid, Some((5.0, 2.5)));
        assert!(matches!(
            h.people[0].state,
            State::AtDesk {
                ticket: Some(0),
                done: false,
                ..
            }
        ));
        let page = h
            .plugin_api("humans.fares.list", &json!({"offset":0,"limit":1}), None)
            .unwrap();
        assert_eq!(page["items"][0]["name"], "Adult");
        assert_eq!(page["next_offset"], 1);
        assert!(h
            .plugin_api("humans.fares.list", &json!({"offset":3}), None)
            .is_err());
    }
    #[test]
    fn stale_human_handles_do_not_resolve_to_reused_native_ids() {
        assert_eq!(parse_handle(&json!("human:12:7"), 12).unwrap(), 7);
        assert!(parse_handle(&json!("human:12:7"), 13).is_err());
        assert!(parse_handle(&json!("human:12:7:extra"), 12).is_err());
        assert!(parse_handle(&json!(7), 12).is_err());
        assert!(parse_handle(&json!("human:12:0"), 12).is_err());
    }
    #[test]
    fn native_human_field_validation_rejects_unknown_and_out_of_range_values() {
        assert!(keys(&json!({"seat":3}), &["id", "entry"]).is_err());
        assert!(number(&json!({"pace":0.0}), "pace", 0.1, 5.0).is_err());
        assert!(number(&json!({"speed":5.1}), "speed", 0.0, 5.0).is_err());
        assert_eq!(
            number(&json!({"speed":0.0}), "speed", 0.0, 5.0).unwrap(),
            Some(0.0)
        );
    }
}
