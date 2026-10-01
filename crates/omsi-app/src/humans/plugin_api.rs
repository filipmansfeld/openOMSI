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
    matches!(state, State::Strolling(_)) || matches!(state, State::Pax(p) if matches!(p.st, 1 | 5))
}

pub(super) fn apply_motion(person: &Person, want: &mut Want, command: MotionCommand) {
    let bus = match person.place {
        Place::Ground => None,
        Place::Bus(bus, _) => Some(bus),
    };
    if person.remote
        || person.puppet.is_some()
        || !matches!(person.state, State::Strolling(_))
        || bus != command.bus
    {
        return;
    }
    let angle = command.heading.to_radians();
    want.vel = DVec2::new(angle.sin(), angle.cos()) * command.speed;
    want.face = Some(command.heading);
    // Keep the native corridor: this command changes desired motion, while
    // collision avoidance still decides actual movement.
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
        State::Idle => json!({"kind":"idle"}),
        State::Standing => json!({"kind":"standing"}),
        State::Pax(x) => json!({"kind":"passenger", "task":x.task.name(), "movement_state":x.st,
            "bus":x.bus.map(bus_id),"inside":x.inside.map(bus_id),"stop_id":x.stop.map(|id|id.to_string()),
            "spot":x.spot,"entry_or_exit":x.door,"seat":x.seat,"destination_stop_name":x.dest,
            "alternate_stop_name":x.alt,"ticket_state":x.ticket,"ticket_index":x.ticket_id.checked_sub(1),
            "payment_step":x.sub,"timer_seconds":x.timer,"path_point":x.pt,"target_path_point":x.pt_target,
            "blocked":x.block,"desired_speed_metres_per_second":x.speed_des}),
    }
}

impl Humans {
    fn plugin_person(&self, p: &Person) -> Value {
        let place = match p.place {
            Place::Ground => json!({"kind":"ground"}),
            Place::Bus(bus, position) => json!({"kind":"bus","bus":bus_id(bus),
                "local_position":{"right":position.x,"forward":position.y,"up":position.z}}),
        };
        let desired = match &p.state {
            State::Pax(x) => Some(
                json!({"speed_metres_per_second":x.speed_des,"heading_degrees":x.yaw.to_degrees()}),
            ),
            _ => self.plugin_desired_motion.get(&p.id).map(|(v, h)| {
                json!({"speed_metres_per_second":v.length(),
                "heading_degrees":h,"velocity":{"right_or_east":v.x,"forward_or_north":v.y}})
            }),
        };
        let pace = match &p.state {
            State::Pax(x) => x.walk_speed as f64,
            _ => p.pace,
        };
        let state_seconds = if matches!(p.state, State::Pax(_)) {
            None
        } else {
            Some(p.t_state)
        };
        json!({"id":format!("human:{}:{}",self.plugin_epoch,p.id),"native_id":p.id,
            "file_name":p.ty.def.path.to_string_lossy(),"state":state(p),"state_seconds":state_seconds,
            "definition":{"height_metres":p.ty.def.height,"mass_kg":p.ty.def.mass,
                "seat_height_metres":p.ty.def.seat_height,"walk_parameters":p.ty.def.walk_param},
            "age_years":p.age,"activity":format!("{:?}",p.activity).to_ascii_lowercase(),
            "position":{"east":p.position.x,"north":p.position.y,"up":p.position.z},
            "heading_degrees":p.heading,"local_heading_degrees":p.lheading,"place":place,
            "velocity":{"right_or_east":p.vel.x,"forward_or_north":p.vel.y},
            "speed_metres_per_second":p.vel.length(),"pace_metres_per_second":pace,
            "desired_motion":desired,"motion_command_pending":self.plugin_motion.contains_key(&p.id),
            "stuck_seconds":p.stuck,"waiting_reason":p.why,
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
        let person = &self.people[index];
        let pax = self.pax(index);
        let decided = pax.is_some_and(|p| {
            !matches!(
                p.task,
                Task::WaitingForBus | Task::ToBus | Task::WalkingToBusstop
            )
        });
        let mode = match pax {
            _ if !decided => "auto",
            Some(p) if p.ticket == TICKET_BUY => "buy",
            Some(p) if p.ticket == TICKET_STAMP => "stamp",
            _ => "pass",
        };
        let ticket = pax
            .and_then(|p| p.ticket_id.checked_sub(1))
            .map(usize::from);
        let product = ticket.and_then(|i| {
            self.tickets.as_ref()?.tickets.get(i).map(|t|
            json!({"index":i,"name":t.name,"value":t.value,"display_string":t.display_string}))
        });
        let transaction = pax.filter(|p| p.inside == Some(BusId::Player) && self.desk_busy == Some(person.id))
            .map(|p| json!({"ticket_index":ticket,"payment_step":p.sub,
                "requested_name":self.request.as_ref().map(|r|&r.0),"requested_value":self.request.as_ref().map(|r|r.1),
                "paid_amount":self.paid.map(|v|v.0),"payment_price":self.paid.map(|v|v.1),
                "change_due":self.change_due,"change_on_tray":self.money.as_ref().map(|m|m.change_value())}));
        json!({"id":format!("human:{}:{}",self.plugin_epoch,person.id),"mode":mode,
            "ticket_index":ticket,"decided":decided,"product":product,"transaction":transaction})
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
        let p = self
            .pax(index)
            .ok_or("ticket choice requires a passenger")?;
        if p.inside.is_some() || self.people[index].place != Place::Ground {
            return Err("ticket choice can change only before boarding; an active payment cannot be rewritten".into());
        }
        if p.task != Task::WalkingToBus {
            return Err("ticket choice is decided when a seat is reserved; wait until the passenger is WalkingToBus".into());
        }
        let bn = p
            .bus
            .and_then(|b| self.last_buses.iter().find(|bn| bn.id == b))
            .ok_or("the passenger's bus is no longer loaded")?
            .clone();
        let door = p
            .door
            .and_then(|d| bn.cabin.entries.get(d))
            .ok_or("the boarding entry is unavailable")?;
        let explicit = if mode == "buy" {
            let i = args
                .get("ticket_index")
                .and_then(Value::as_u64)
                .filter(|i| *i < 255)
                .ok_or("buy requires a ticket_index in 0..254")? as usize;
            let t = self
                .tickets
                .as_ref()
                .and_then(|pack| pack.tickets.get(i))
                .ok_or("ticket_index is not in the active catalogue")?;
            if !t.value.is_finite()
                || t.value < 0.0
                || self.people[index].age < t.age_min as f32
                || self.people[index].age > t.age_max as f32
            {
                return Err("the selected fare is invalid for this passenger".into());
            }
            if !door.sells || bn.cabin.sale.is_none() {
                return Err("select a selling entry before changing the fare".into());
            }
            Some((TICKET_BUY, (i + 1) as u8))
        } else {
            if args.get("ticket_index").is_some() {
                return Err("ticket_index is allowed only for buy mode".into());
            }
            if mode == "stamp" && bn.cabin.stamper.is_none() {
                return Err("the bus has no usable validator".into());
            }
            match mode {
                "stamp" => Some((TICKET_STAMP, 0)),
                "pass" => Some((TICKET_NONE, 0)),
                _ => None,
            }
        };
        let (ticket, ticket_id) = explicit.unwrap_or_else(|| self.decide_pax_ticket(index, &bn));
        let p = self.pax_mut(index).unwrap();
        p.ticket = ticket;
        p.ticket_id = ticket_id;
        // The native selection chooses a selling door for buyers on its next tick.
        let buses = self.last_buses.clone();
        let bus_ix = buses.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
        self.choose_entry(index, &buses, &bus_ix);
        Ok(self.plugin_ticket(index))
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
        if args.get("waiting_patience_seconds").is_some() {
            return Err("waiting_patience_seconds is unavailable in the native passenger task model; read state.task and state.timer_seconds instead".into());
        }
        let pace = number(args, "pace_metres_per_second", 0.1, 5.0)?;
        let speed = number(args, "speed_metres_per_second", 0.0, 5.0)?;
        let heading = number(args, "heading_degrees", -360000.0, 360000.0)?;
        let elapsed = number(args, "state_seconds", 0.0, 86400.0)?;
        let exit = args
            .get("exit_stop")
            .map(|v| v.as_i64().ok_or("exit_stop must be an integer"))
            .transpose()?;
        let p = &self.people[index];
        if elapsed.is_some() && matches!(p.state, State::Pax(_)) {
            return Err("state_seconds is unavailable for native passenger tasks; read state.timer_seconds instead".into());
        }
        if speed.is_some() && !walking(&p.state) {
            return Err("instantaneous speed is writable only while walking".into());
        }
        let destination = if let Some(exit) = exit {
            let pax = self.pax(index).ok_or("exit_stop requires a passenger")?;
            if pax.inside != Some(BusId::Player)
                || !matches!(pax.task, Task::SittingInBus | Task::InBusToPlace)
            {
                return Err(
                    "exit_stop is writable only aboard the player's bus before alighting".into(),
                );
            }
            if exit == -1 {
                None
            } else {
                let stops = stop_ids.ok_or("the player has no current timetable stop list")?;
                let id = usize::try_from(exit)
                    .ok()
                    .and_then(|i| stops.get(i))
                    .ok_or("exit_stop does not identify a current timetable stop")?;
                let name = self
                    .stops
                    .get(id)
                    .map(|s| s.name.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or("the destination stop is not loaded; wait for its native stop record")?;
                if stops
                    .iter()
                    .filter(|other| self.stops.get(other).is_some_and(|s| s.name.trim() == name))
                    .count()
                    > 1
                {
                    return Err("the native passenger model identifies destinations by name; this route has ambiguous stop names".into());
                }
                Some(name.to_string())
            }
        } else {
            None
        };
        if pace.is_none()
            && speed.is_none()
            && heading.is_none()
            && elapsed.is_none()
            && exit.is_none()
        {
            return Err("no human fields were supplied".into());
        }
        let local_heading = heading
            .map(|h| match p.place {
                Place::Ground => Ok(h.rem_euclid(360.0)),
                Place::Bus(bus, local) => self
                    .last_buses
                    .iter()
                    .find(|b| b.id == bus)
                    .map(|b| (h - b.heading_at(local)).rem_euclid(360.0))
                    .ok_or("the person's bus pose is not available"),
            })
            .transpose()?;
        let ride = if exit == Some(-1) {
            Some((self.rand_f() * 19.0 + 1.0) as f32)
        } else {
            None
        };
        let km = self.odometer.get(&BusId::Player).copied().unwrap_or(0.0);
        let p = &mut self.people[index];
        if let Some(v) = pace {
            p.pace = v;
        }
        if let Some(v) = heading {
            p.heading = v.rem_euclid(360.0);
        }
        if let Some(v) = local_heading {
            p.lheading = v;
        }
        if let Some(v) = elapsed {
            p.t_state = v as f32;
        }
        if let Some(v) = speed {
            let h = if p.place == Place::Ground {
                p.heading
            } else {
                p.lheading
            }
            .to_radians();
            let dir = if p.vel.length_squared() > 1e-12 {
                p.vel.normalize()
            } else {
                DVec2::new(h.sin(), h.cos())
            };
            p.vel = dir * v;
        }
        if let State::Pax(x) = &mut p.state {
            if let Some(v) = pace {
                x.walk_speed = v as f32;
            }
            if let Some(v) = speed {
                x.speed = v as f32;
            }
            if let Some(v) = local_heading {
                x.yaw = v.to_radians();
            }
            if exit.is_some() {
                x.dest = destination;
                x.alt = None;
                x.alt_seen = false;
            }
            if let Some(v) = ride {
                x.ride_km = v;
                x.km_start = km;
            }
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
        if !matches!(p.state, State::Strolling(_)) {
            return Err("desired motion is available for strolling pedestrians; passenger motion belongs to the native task/path controller".into());
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
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("seat must be a non-negative passenger place index")?;
        let p = self
            .pax(index)
            .filter(|p| p.task == Task::SittingInBus)
            .ok_or("seat reassignment requires a riding passenger")?;
        let (bus, old) = p
            .inside
            .zip(p.seat)
            .ok_or("the rider has no bus/place reservation")?;
        if !matches!(self.people[index].place,Place::Bus(b,_) if b==bus) || p.ticket >= TICKET_STAMP
        {
            return Err("the passenger is not ready to change places".into());
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
            .ok_or("the current place is not in this cabin")?;
        let target = bn
            .cabin
            .seats
            .get(seat)
            .ok_or("the requested place is not in this cabin")?;
        let taken = self
            .seats
            .get(&bus)
            .ok_or("the bus has no live reservation table")?;
        if taken.len() != bn.cabin.seats.len() || !taken.get(old).copied().unwrap_or(false) {
            return Err("the current place is not reserved".into());
        }
        if seat == old {
            return Ok(());
        }
        if taken[seat] {
            return Err("the requested place is already reserved".into());
        }
        if self.people.iter().enumerate().any(|(other,p)|other!=index && matches!(&p.state,State::Pax(x) if x.bus.or(x.inside)==Some(bus) && x.seat.is_some_and(|s|s==old||s==seat))) {
            return Err("the reservation is inconsistent with another passenger".into());
        }
        let points = bn.cabin.all_points();
        let start = bn
            .cabin
            .omsi_nearest(current.floor, &points, false, true, None, None)
            .ok_or("the current place has no cabin path")?;
        let end = bn
            .cabin
            .omsi_nearest(target.floor, &points, false, true, None, None)
            .ok_or("the requested place has no cabin path")?;
        if start != end && bn.cabin.route_next(start, end).is_none() {
            return Err("the cabin has no walking route to the requested place".into());
        }
        let mut position = p.pos;
        if current.seated {
            position.z = current.floor.z as f64;
        }
        let taken = self.seats.get_mut(&bus).unwrap();
        taken[seat] = true;
        taken[old] = false;
        let x = self.pax_mut(index).unwrap();
        x.seat = Some(seat);
        x.task = Task::InBusToPlace;
        x.st = 5;
        x.pt = Some(start);
        x.pt_target = Some(end);
        x.pos = position;
        x.short = false;
        x.smooth = false;
        x.pax_state = 1.0;
        x.speed = 0.0;
        x.sub = 0;
        let p = &mut self.people[index];
        p.place = Place::Bus(bus, position.as_vec3());
        p.vel = DVec2::ZERO;
        p.activity = Activity::Stand;
        p.t_state = 0.0;
        Ok(())
    }

    fn plugin_reassign_entry(&mut self, index: usize, args: &Value) -> Result<(), String> {
        keys(args, &["id", "entry"])?;
        self.plugin_writable(index)?;
        let entry = args
            .get("entry")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("entry must be a non-negative integer")?;
        let p = self
            .pax(index)
            .filter(|p| p.task == Task::WalkingToBus && p.inside.is_none())
            .ok_or("entry reassignment requires a passenger walking to a bus")?;
        if self.people[index].place != Place::Ground {
            return Err("the passenger must still be on the ground".into());
        }
        let bus = p.bus.ok_or("the passenger has no boarding bus")?;
        let stop = p.stop.ok_or("the passenger has no boarding stop")?;
        let bn = self
            .last_buses
            .iter()
            .find(|b| b.id == bus)
            .ok_or("the boarding bus is no longer loaded")?;
        if bn.speed.abs() >= 0.5 || !self.in_stop_box(stop, bus) {
            return Err("the bus must stand in the passenger's stop box".into());
        }
        let door = bn
            .cabin
            .entries
            .get(entry)
            .ok_or("entry is not part of this cabin")?;
        if !bn.entry_open.get(entry.min(7)).copied().unwrap_or(false) {
            return Err("the requested entry is not open".into());
        }
        if p.ticket == TICKET_BUY && !door.sells {
            return Err("this passenger needs a selling entry".into());
        }
        let seat = p.seat.ok_or("the passenger has no reserved place")?;
        if !self
            .seats
            .get(&bus)
            .and_then(|s| s.get(seat))
            .copied()
            .unwrap_or(false)
        {
            return Err("the passenger's place is no longer reserved".into());
        }
        let target = door
            .point
            .and_then(|k| bn.cabin.graph.points.get(k))
            .copied()
            .ok_or("entry has no native path point")?;
        let p = self.pax_mut(index).unwrap();
        p.door = Some(entry);
        p.target = target.as_dvec3();
        p.target_bus = true;
        p.short = false;
        p.st = 1;
        p.pax_state = 1.0;
        self.people[index].vel = DVec2::ZERO;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omsi_content::tickets::{Ticket, TicketPack};

    fn stop(name: &str) -> PaxStop {
        PaxStop {
            name: name.into(),
            pos: DVec3::ZERO,
            heading: 0.0,
            gather: DVec3::ZERO,
            spots: vec![WaitSpot {
                pos: DVec3::ZERO,
                face: 0.0,
                height: 0.0,
            }],
            taken: vec![false],
            enter_max: 1.0,
            enter_min: 0.0,
            length: 30.0,
            lane: None,
            was_near: true,
            near: true,
            clock_ms: 0.0,
            want: 1,
            factor: 1.0,
            buses: vec![(BusId::Player, true)],
            dests: vec![],
            lines: vec![],
        }
    }

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
        let mut pax = Pax::new(1.4);
        pax.task = Task::WalkingToBus;
        pax.st = 1;
        pax.bus = Some(BusId::Player);
        pax.stop = Some(7);
        pax.spot = None;
        pax.seat = Some(0);
        pax.door = Some(0);
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
            anim: OmsiAnim::default(),
            state: State::Pax(Box::new(pax)),
            t_state: 0.0,
            skins: vec![],
            interior: 0.0,
            lit: 0.0,
            tilt: Mat4::IDENTITY,
            age: 40.0,
            stuck: 0.0,
            ghost: 0.0,
            car_wait: 0.0,
            detour: 0.0,
            detour_side: 0.0,
            why: "",
            skinned: false,
            since_posed: 0,
            posed_at: (DVec3::ZERO, 0.0),
            ankles: [Vec3::ZERO; 2],
            puppet: None,
            remote: false,
        });
        let door = Door {
            inside: Vec3::ZERO,
            point: Some(0),
            outside: Vec3::ZERO,
            side: 1.0,
            queue_dir: 1.0,
            sells: true,
            button: false,
            wait: Vec3::ZERO,
        };
        let points = vec![Vec3::ZERO, Vec3::new(0.0, 2.0, 0.0)];
        let links = vec![(0, 1, false)];
        let cabin = Arc::new(Cabin {
            data: PassengerCabin::default(),
            graph: PathGraph::new(points, &links),
            routes: build_routes(2, &links),
            links,
            link_pack: vec![None],
            step_packs: vec![],
            entries: vec![
                door.clone(),
                Door {
                    point: Some(1),
                    inside: Vec3::new(0.0, 2.0, 0.0),
                    ..door
                },
            ],
            exits: vec![],
            desk: None,
            seats: vec![
                Seat {
                    pos: Vec3::new(0.0, 0.0, 0.8),
                    floor: Vec3::ZERO,
                    rot: 0.0,
                    seated: true,
                    height: 0.8,
                },
                Seat {
                    pos: Vec3::new(0.0, 2.0, 0.8),
                    floor: Vec3::new(0.0, 2.0, 0.0),
                    rot: 0.0,
                    seated: true,
                    height: 0.8,
                },
            ],
            parts: vec![],
            link_room: vec![2.0],
            stamper: Some((Some(0), Vec3::ZERO)),
            sale: Some((Some(0), Vec3::ZERO)),
            money_point: None,
            money_var: None,
            change_point: None,
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
            interior: 0.0,
            air: CabinAir::default(),
            half: DVec2::ONE,
            centre: DVec2::ZERO,
            accel: DVec2::ZERO,
            trailers: vec![],
            terminus: Some("Test".into()),
        });
        h.seats.insert(BusId::Player, vec![true, false]);
        h.stops.insert(7, stop("Origin"));
        h.stops.insert(8, stop("Destination"));
        h
    }

    fn identity(h: &Humans) -> String {
        format!("human:{}:1", h.plugin_epoch)
    }

    fn aboard(h: &mut Humans) {
        h.people[0].place = Place::Bus(BusId::Player, Vec3::new(0.0, 0.0, 0.8));
        let p = h.pax_mut(0).unwrap();
        p.inside = Some(BusId::Player);
        p.task = Task::SittingInBus;
        p.st = 0;
        p.pos = DVec3::new(0.0, 0.0, 0.8);
    }

    fn tickets() -> Arc<TicketPack> {
        Arc::new(TicketPack {
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
        })
    }

    #[test]
    fn native_human_batches_and_destination_mapping_are_atomic() {
        let mut h = fixture();
        aboard(&mut h);
        let id = identity(&h);
        let before = h.plugin_api("humans.get", &json!({"id":id}), None).unwrap();
        for args in [
            json!({"id":id,"pace_metres_per_second":2.0,"exit_stop":99}),
            json!({"id":id,"pace_metres_per_second":2.0,"waiting_patience_seconds":30.0}),
            json!({"id":id,"pace_metres_per_second":2.0,"state_seconds":30.0}),
            json!({"id":id,"pace_metres_per_second":2.0,"unsupported":1}),
        ] {
            assert!(h.plugin_api("humans.set", &args, Some(&[7, 8])).is_err());
            assert_eq!(
                h.plugin_api("humans.get", &json!({"id":id}), None).unwrap(),
                before
            );
        }
        h.plugin_api(
            "humans.set",
            &json!({"id":id,"pace_metres_per_second":2.0,"exit_stop":1}),
            Some(&[7, 8]),
        )
        .unwrap();
        assert_eq!(h.people[0].pace, 2.0);
        assert_eq!(h.pax(0).unwrap().walk_speed, 2.0);
        assert_eq!(h.pax(0).unwrap().dest.as_deref(), Some("Destination"));
        h.stops.insert(9, stop(" Destination "));
        for route in [&[8, 9][..], &[8, 8][..], &[99][..]] {
            assert!(h
                .plugin_api("humans.set", &json!({"id":id,"exit_stop":0}), Some(route))
                .is_err());
            assert_eq!(h.pax(0).unwrap().dest.as_deref(), Some("Destination"));
        }
        h.odometer.insert(BusId::Player, 12.0);
        h.plugin_api("humans.set", &json!({"id":id,"exit_stop":-1}), None)
            .unwrap();
        let p = h.pax(0).unwrap();
        assert!(p.dest.is_none());
        assert!((1.0..=20.0).contains(&p.ride_km));
        assert_eq!(p.km_start, 12.0);
    }

    #[test]
    fn native_entry_reassignment_preserves_reserved_seat_and_released_waiting_place() {
        let mut h = fixture();
        let id = identity(&h);
        h.last_buses[0].entry_open[1] = false;
        assert!(h
            .plugin_api("humans.reassign_entry", &json!({"id":id,"entry":1}), None)
            .is_err());
        assert_eq!(h.pax(0).unwrap().door, Some(0));
        h.last_buses[0].entry_open[1] = true;
        h.plugin_api("humans.reassign_entry", &json!({"id":id,"entry":1}), None)
            .unwrap();
        let p = h.pax(0).unwrap();
        assert_eq!(p.door, Some(1));
        assert_eq!(p.target, DVec3::new(0.0, 2.0, 0.0));
        assert_eq!(p.task, Task::WalkingToBus);
        assert_eq!(p.st, 1);
        assert!(p.target_bus);
        assert_eq!(h.seats[&BusId::Player], vec![true, false]);
        assert_eq!(p.spot, None);
        assert_eq!(h.stops[&7].taken, vec![false]);
        h.seats.get_mut(&BusId::Player).unwrap()[0] = false;
        assert!(h
            .plugin_api("humans.reassign_entry", &json!({"id":id,"entry":0}), None)
            .is_err());
        assert_eq!(h.pax(0).unwrap().door, Some(1));
    }

    #[test]
    fn native_seat_assignment_reserves_then_walks_without_horizontal_teleport() {
        let mut h = fixture();
        aboard(&mut h);
        let id = identity(&h);
        h.seats.get_mut(&BusId::Player).unwrap()[1] = true;
        assert!(h
            .plugin_api("humans.assign_seat", &json!({"id":id,"seat":1}), None)
            .is_err());
        assert_eq!(h.pax(0).unwrap().seat, Some(0));
        h.seats.get_mut(&BusId::Player).unwrap()[1] = false;
        let old_position = h.pax(0).unwrap().pos;
        h.plugin_api("humans.assign_seat", &json!({"id":id,"seat":1}), None)
            .unwrap();
        let p = h.pax(0).unwrap();
        assert_eq!(p.seat, Some(1));
        assert_eq!(p.task, Task::InBusToPlace);
        assert_eq!((p.st, p.pt, p.pt_target), (5, Some(0), Some(1)));
        assert_eq!(p.pos.truncate(), old_position.truncate());
        assert_eq!(p.pos.z, 0.0);
        assert_eq!(h.people[0].place, Place::Bus(BusId::Player, Vec3::ZERO));
        assert_eq!(h.seats[&BusId::Player], vec![false, true]);
    }

    #[test]
    fn native_ticket_choice_uses_one_based_pax_ids_and_native_entry_selection() {
        let mut h = fixture();
        let id = identity(&h);
        h.tickets = Some(tickets());
        Arc::get_mut(&mut h.last_buses[0].cabin).unwrap().entries[0].sells = false;
        assert!(h
            .plugin_api(
                "humans.ticket.set",
                &json!({"id":id,"mode":"buy","ticket_index":0}),
                None
            )
            .is_err());
        h.plugin_api("humans.reassign_entry", &json!({"id":id,"entry":1}), None)
            .unwrap();
        h.plugin_api(
            "humans.ticket.set",
            &json!({"id":id,"mode":"buy","ticket_index":0}),
            None,
        )
        .unwrap();
        let p = h.pax(0).unwrap();
        assert_eq!((p.ticket, p.ticket_id, p.door), (TICKET_BUY, 1, Some(1)));
        assert_eq!(h.plugin_ticket(0)["ticket_index"], 0);
        let before = h.plugin_ticket(0);
        assert!(h
            .plugin_api(
                "humans.ticket.set",
                &json!({"id":id,"mode":"buy","ticket_index":1}),
                None
            )
            .is_err());
        assert_eq!(h.plugin_ticket(0), before);
        h.plugin_api("humans.ticket.set", &json!({"id":id,"mode":"pass"}), None)
            .unwrap();
        assert_eq!(
            (h.pax(0).unwrap().ticket_id, h.pax(0).unwrap().door),
            (0, Some(0))
        );
        h.plugin_api("humans.ticket.set", &json!({"id":id,"mode":"stamp"}), None)
            .unwrap();
        assert_eq!(h.pax(0).unwrap().ticket, TICKET_STAMP);
        aboard(&mut h);
        h.desk_busy = Some(1);
        h.paid = Some((5.0, 2.5));
        assert!(h
            .plugin_api("humans.ticket.set", &json!({"id":id,"mode":"pass"}), None)
            .is_err());
        assert_eq!(h.plugin_ticket(0)["transaction"]["paid_amount"], 5.0);
        assert_eq!(h.paid, Some((5.0, 2.5)));
    }

    #[test]
    fn native_ticket_catalogue_remaps_one_based_ids_and_preserves_active_payment() {
        let mut h = fixture();
        let old = tickets();
        let mut reordered = (*old).clone();
        reordered.tickets.swap(0, 1);
        reordered.tickets[1].value = 3.0;
        let next = Some(Arc::new(reordered));
        h.tickets = Some(old.clone());
        let p = h.pax_mut(0).unwrap();
        p.ticket = TICKET_BUY;
        p.ticket_id = 1;
        h.paid = Some((2.5, 2.5));
        assert!(h.preflight_ticket_pack(&next).is_err());
        assert!(!h.preflight_ticket_pack(&Some(old)).unwrap());
        assert_eq!(h.pax(0).unwrap().ticket_id, 1);
        h.paid = None;
        h.desk_busy = Some(1);
        assert!(h.preflight_ticket_pack(&next).is_err());
        h.desk_busy = None;
        assert!(h.preflight_ticket_pack(&next).unwrap());
        h.commit_ticket_pack(next);
        assert_eq!(h.pax(0).unwrap().ticket_id, 2);
        assert_eq!(h.plugin_ticket(0)["product"]["name"], "Adult");
        h.commit_ticket_pack(None);
        assert_eq!(
            (h.pax(0).unwrap().ticket, h.pax(0).unwrap().ticket_id),
            (TICKET_NONE, 0)
        );
    }

    #[test]
    fn native_motion_control_rejects_passengers_and_preserves_pedestrian_corridor() {
        let mut h = fixture();
        let id = identity(&h);
        let args = json!({"id":id,"speed_metres_per_second":2.0,"heading_degrees":90.0});
        assert!(h.plugin_api("humans.control", &args, None).is_err());
        assert!(h.plugin_motion.is_empty());
        h.people[0].state = State::Strolling(PedWalk::new(vec![], true, 0.0));
        h.plugin_api("humans.control", &args, None).unwrap();
        assert_eq!(h.people[0].vel, DVec2::ZERO);
        let command = h.plugin_motion.remove(&1).unwrap();
        let corridor = (DVec2::ZERO, DVec2::ONE, 0.5);
        let mut want = Want::stand(None, Activity::Stand);
        want.corridor = Some(corridor);
        apply_motion(&h.people[0], &mut want, command);
        assert!((want.vel - DVec2::new(2.0, 0.0)).length() < 1e-9);
        assert_eq!(want.face, Some(90.0));
        assert_eq!(want.corridor, Some(corridor));
        assert!(parse_handle(
            &json!(format!("human:{}:1", h.plugin_epoch + 1)),
            h.plugin_epoch
        )
        .is_err());
    }
}
