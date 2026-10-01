//! Native timetable data and duty commands, shared by Lua and bridge callers.
use crate::App;
use serde_json::{json, Value};

/// Prepare every fallible catalog/duty change before touching the running clock.
/// Date transitions retire the old scheduled fleet rather than leave its indices
/// pointing into a replacement timetable. A changed Chrono graph also rebuilds
/// random traffic and ground pedestrians before acknowledging the transition.
pub(crate) fn set_clock(
    app: &mut App,
    update: &crate::native_bridge::protocol::ClockUpdate,
) -> Result<(bool, Option<String>), String> {
    let (year, day) = update.validate(app.clock.year, app.clock.day_of_year)?;
    let mut target = app.clock.clone();
    target.year = year;
    target.day_of_year = day;
    if let Some(time) = update.service_seconds {
        target.time = time;
    }
    let date_changed = target.date_code() != app.clock.date_code();
    let mut replacement = None;
    let mut replacement_traffic = None;
    let mut replacement_tickets = None;
    let mut graph_changed = false;
    let mut planned_duty = None;
    let mut duty_cleared = None;
    if date_changed {
        if app.lan.is_some() {
            return Err("calendar changes in a LAN session require coordinated host/map reload; this operation cannot apply them locally".into());
        }
        if let Some(world) = app.world.as_ref() {
            if app.renderer.is_none() || app.scene.is_none() || app.streamer.is_none() {
                return Err(
                    "date changes require the map renderer and tile streamer to be ready".into(),
                );
            }
            let ticket_path = world.ticket_pack_on_date(target.date_code());
            if ticket_path != world.ticket_pack() {
                let pack = if ticket_path.trim().is_empty() {
                    None
                } else {
                    let path = omsi_cfg::resolve_path(&app.args.root, &ticket_path);
                    Some(std::sync::Arc::new(
                        omsi_content::tickets::TicketPack::load(&path).map_err(|e| {
                            format!("target date ticket pack failed preflight: {e}")
                        })?,
                    ))
                };
                if let Some(humans) = app.humans.as_ref() {
                    humans.preflight_ticket_pack(&pack)?;
                }
                replacement_tickets = Some(pack);
            }
            let target_chrono = omsi_map::active_chrono_dirs(&world.map_dir, target.date_code());
            graph_changed = target_chrono != *world.chrono_dirs.read();
            if graph_changed {
                if let Some(old) = app.traffic.as_ref() {
                    let mut traffic = crate::traffic::Traffic::prepare_date_reset(
                        &app.args.root,
                        world,
                        old.target,
                        target.date_code(),
                    )
                    .map_err(|e| format!("target date traffic failed preflight: {e}"))?;
                    traffic.spawn_radius = old.spawn_radius;
                    traffic.time_scale = old.time_scale;
                    traffic.unsched_factor = old.unsched_factor;
                    traffic.max_scheduled = old.max_scheduled;
                    traffic.player_priority = old.player_priority;
                    traffic.day_time = target.time;
                    if let Some(weather) = app.weather.as_ref() {
                        traffic.set_weather(weather, app.wetness);
                    }
                    replacement_traffic = Some(traffic);
                }
            }
            if let Some(old) = app.schedule.as_ref() {
                let mut schedule = crate::schedule::Schedule::new(&app.args.root, world, &target);
                if let Some(error) = schedule
                    .data
                    .errors
                    .iter()
                    .find(|error| !old.data.errors.contains(error))
                {
                    return Err(format!("target date timetable failed preflight: {error}"));
                }
                if let Some(previous) = app.duty.as_ref() {
                    match schedule
                        .require_tour(&previous.line, &previous.tour)
                        .and_then(|()| {
                            schedule.player_duty(
                                world,
                                &previous.line,
                                &previous.tour,
                                target.time,
                                None,
                                false,
                            )
                        }) {
                        Ok(duty) => planned_duty = Some(duty),
                        Err(error) => duty_cleared = Some(error),
                    }
                }
                replacement = Some(schedule);
            }
        }
    }
    let previous_date = app.clock.date_code();
    let time_delta = target.time - app.clock.time;
    app.clock = target;
    if date_changed {
        // Retire native graph users while the old graph still exists. Eviction
        // releases their seats/door queues before the cars and paths disappear.
        if let (Some(world), Some(renderer), Some(scene), Some(traffic)) = (
            app.world.as_ref(),
            app.renderer.as_ref(),
            app.scene.as_mut(),
            app.traffic.as_mut(),
        ) {
            let ids = if graph_changed {
                traffic.cars.iter().map(|car| car.id).collect()
            } else {
                app.schedule
                    .as_ref()
                    .map(|s| s.scheduled_vehicle_ids())
                    .unwrap_or_default()
            };
            for id in ids {
                if let Some(humans) = app.humans.as_mut() {
                    humans.evict(crate::humans::BusId::Ai(id), world);
                }
                traffic.remove_car(world, renderer, scene, id);
            }
            traffic.day_time = app.clock.time;
        }
        if graph_changed {
            if let Some(humans) = app.humans.as_mut() {
                humans.invalidate_traffic_network();
            }
            for player in app.player.iter_mut().chain(app.placed.iter_mut()) {
                player.rail = None;
            }
        }
        if let Some(mut schedule) = replacement {
            schedule.synchronize_clock(&app.clock, app.clock.time);
            if let Some(humans) = app.humans.as_mut() {
                humans.stop_targets = Some(schedule.stop_targets());
            }
            app.schedule = Some(schedule);
            app.duty = planned_duty;
            if app.duty.is_none() {
                crate::game_lists::clear_duty(app);
            }
        } else if let Some(traffic) = app.traffic.as_mut() {
            traffic.day_time = app.clock.time;
        }
        // The normal date-following path owns texture invalidation and tile reloads.
        // Preserve the old date if this is the first frame before it had recorded one.
        if app.world.is_some() {
            app.world_day
                .get_or_insert_with(|| (previous_date, omsi_texture::season_folder()));
            app.follow_date();
            if let Some(pack) = replacement_tickets {
                for player in app.player.iter_mut().chain(app.placed.iter_mut()) {
                    player.vehicle.host.tickets = pack.clone();
                }
                if let Some(humans) = app.humans.as_mut() {
                    humans.commit_ticket_pack(pack);
                }
            }
            if graph_changed {
                let world = app.world.as_ref().expect("preflight retained world");
                world.reset_traffic_sources();
                app.streamer
                    .as_mut()
                    .expect("preflight retained streamer")
                    .reload(
                        app.renderer.as_ref().expect("preflight retained renderer"),
                        app.scene.as_mut().expect("preflight retained scene"),
                        None,
                        app.audio.as_ref(),
                    );
                if let (Some(traffic), Some(audio)) = (app.traffic.as_mut(), app.audio.as_ref()) {
                    traffic.stop_audio(audio);
                }
                if let (Some(old), Some(replacement)) =
                    (app.traffic.as_mut(), replacement_traffic.as_mut())
                {
                    old.transfer_reset_resources(
                        world,
                        app.renderer.as_ref().expect("preflight retained renderer"),
                        app.scene.as_mut().expect("preflight retained scene"),
                        replacement,
                    );
                }
                app.traffic = replacement_traffic;
            }
            if let Some(world) = app.world.as_ref() {
                let _ = world.index();
                let positions = world.object_positions.lock();
                if let Some(duty) = app.duty.as_mut() {
                    duty.refresh_places(|id| positions.get(&id).map(|(p, _)| *p));
                }
            }
            if let Some(navigator) = app.navigator.as_mut() {
                navigator.invalidate_map();
            }
            app.duty_places = false;
            update_destination(app);
        }
    } else {
        if let Some(traffic) = app.traffic.as_mut() {
            traffic.day_time += time_delta;
        }
        if let Some(schedule) = app.schedule.as_mut() {
            schedule.synchronize_clock(
                &app.clock,
                app.traffic
                    .as_ref()
                    .map(|t| t.day_time)
                    .unwrap_or(app.clock.time),
            );
        }
    }
    for player in app.player.iter_mut().chain(app.placed.iter_mut()) {
        player.vehicle.host.clock = app.clock.clone();
    }
    Ok((date_changed, duty_cleared))
}

pub(crate) fn execute(
    app: &mut App,
    operation: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if !operation.starts_with("timetable.") || operation == "timetable.get" {
        return None;
    }
    Some(run(app, operation, args))
}

fn index(args: &Value, name: &str) -> Result<usize, String> {
    let number = args
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{name} must be a nonnegative integer"))?;
    usize::try_from(number).map_err(|_| format!("{name} is out of range"))
}

fn name<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or_else(|| format!("{field} must be a nonempty name"))
}

fn page<T>(items: &[T], args: &Value, make: impl Fn(usize, &T) -> Value) -> Result<Value, String> {
    let offset = if args.get("offset").is_some() {
        index(args, "offset")?
    } else {
        0
    };
    let limit = if args.get("limit").is_some() {
        index(args, "limit")?
    } else {
        100
    };
    if !(1..=256).contains(&limit) {
        return Err("limit must be between 1 and 256".into());
    }
    if offset > items.len() {
        return Err("offset is beyond the current catalog".into());
    }
    let end = offset.saturating_add(limit).min(items.len());
    let values: Vec<_> = items[offset..end]
        .iter()
        .enumerate()
        .map(|(i, v)| make(offset + i, v))
        .collect();
    Ok(
        json!({"items":values,"total":items.len(),"next_offset":if end<items.len(){Some(end)}else{None}}),
    )
}

fn update_destination(app: &mut App) {
    if let (Some(duty), Some(player)) = (&mut app.duty, &mut app.player) {
        duty.update(&mut player.vehicle, app.clock.time);
        let (trip, stop) = duty.trip_for_ibis();
        player.set_duty_destination(trip, stop);
    }
    reconcile_passenger_destinations(app);
}

fn reconcile_passenger_destinations(app: &mut App) {
    if let Some(humans) = app.humans.as_mut() {
        humans.reconcile_player_stop_ids(
            app.player
                .as_ref()
                .map(|p| p.vehicle.host.tt_stop_ids.as_slice())
                .unwrap_or(&[]),
        );
    }
}

fn duty(app: &App) -> Value {
    let Some(d) = app.duty.as_ref() else {
        return Value::Null;
    };
    json!({"line":d.line,"tour":d.tour,"trip_index":d.trip_index,"next_stop":d.next_stop,
        "delay_seconds":d.delay(app.clock.time),"trips":d.trips.iter().map(|trip|json!({
            "name":trip.name,"line":trip.line,"terminus":trip.terminus,
            "departure_seconds":trip.departure,"end_seconds":trip.end,
            "stops":trip.stops.iter().map(|stop|json!({"object_id":stop.object_id.to_string(),
                "name":stop.name,"arrival_seconds":stop.arr,"departure_seconds":stop.dep,"stops":stop.stops,
                "position":stop.position.map(|p|json!({"east":p.x,"north":p.y,"up":p.z})),
                "inbound":stop.dir.inbound.map(|p|json!({"east":p.x,"north":p.y})),
                "outbound":stop.dir.outbound.map(|p|json!({"east":p.x,"north":p.y}))})).collect::<Vec<_>>()
        })).collect::<Vec<_>>()})
}

fn duty_receipt(app: &App) -> Value {
    app.duty
        .as_ref()
        .map(|d| {
            json!({"line":d.line,"tour":d.tour,"trip_index":d.trip_index,
        "next_stop":d.next_stop,"trip_count":d.trips.len()})
        })
        .unwrap_or(Value::Null)
}

fn run(app: &mut App, operation: &str, args: &Value) -> Result<Value, String> {
    let fields: Option<&[&str]> = match operation {
        "timetable.assign" => Some(&["line", "tour"]),
        "timetable.clear" => Some(&[]),
        "timetable.skip_stop" => Some(&["index"]),
        "timetable.start_at" => Some(&["trip_index", "stop_index"]),
        _ => None,
    };
    if let Some(fields) = fields {
        if let Some(name) = args
            .as_object()
            .ok_or("arguments must be an object")?
            .keys()
            .find(|name| name.as_str() != "session_id" && !fields.contains(&name.as_str()))
        {
            return Err(format!("unsupported timetable argument: {name}"));
        }
    }
    match operation {
        "timetable.duty" => return Ok(duty(app)),
        "timetable.assign" => {
            let (line, tour) = (name(args, "line")?, name(args, "tour")?);
            crate::game_lists::assign_duty(app, line, tour)?;
            reconcile_passenger_destinations(app);
            return Ok(duty_receipt(app));
        }
        "timetable.clear" => {
            crate::game_lists::clear_duty(app);
            reconcile_passenger_destinations(app);
            return Ok(Value::Null);
        }
        "timetable.skip_stop" => {
            let stop = index(args, "index")?;
            let d = app.duty.as_mut().ok_or("no active duty")?;
            if stop >= d.trip().stops.len() {
                return Err("stop index is out of range".into());
            }
            // The page helper also permits moving backwards. Keep this API's
            // forward-only contract; deliberate repositioning uses start_at.
            if stop <= d.next_stop || !d.skip_to(stop) {
                return Err("the duty cannot skip backwards or after its last stop".into());
            }
            update_destination(app);
            return Ok(duty_receipt(app));
        }
        "timetable.start_at" => {
            let (trip, stop) = (index(args, "trip_index")?, index(args, "stop_index")?);
            let d = app.duty.as_mut().ok_or("no active duty")?;
            let t = d.trips.get(trip).ok_or("trip index is out of range")?;
            if stop >= t.stops.len() {
                return Err("stop index is out of range".into());
            }
            d.start_at(trip, stop);
            update_destination(app);
            return Ok(duty_receipt(app));
        }
        _ => {}
    }
    let data = &app.schedule.as_ref().ok_or("timetable is not loaded")?.data;
    match operation {
        "timetable.lines" => page(&data.lines, args, |i, line| {
            json!({"index":i,"name":line.name,
            "path":line.path.to_string_lossy(),"user_allowed":line.user_allowed,"priority":line.priority,"tour_count":line.tours.len()})
        }),
        "timetable.line" => {
            let i = index(args, "index")?;
            let l = data.lines.get(i).ok_or("line index is out of range")?;
            Ok(
                json!({"index":i,"name":l.name,"path":l.path.to_string_lossy(),"user_allowed":l.user_allowed,"priority":l.priority,
                "tours":l.tours.iter().enumerate().map(|(ti,t)|json!({"index":ti,"number":t.number,"ai_group":t.ai_group,"extra":t.extra,
                    "trips":t.trips.iter().map(|trip|json!({"trip":trip.trip,"profile":trip.profile,
                        "departure_minutes":trip.departure})).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
            )
        }
        "timetable.trips" => page(&data.trips, args, |i, t| {
            json!({"index":i,"name":t.name,"line":t.line,
            "terminus":t.terminus,"station_count":t.stations.len(),"profile_count":t.profiles.len()})
        }),
        "timetable.trip" => {
            let i = index(args, "index")?;
            let t = data.trips.get(i).ok_or("trip index is out of range")?;
            Ok(
                json!({"index":i,"name":t.name,"path":t.path.to_string_lossy(),"display_name":t.display_name,
                "line":t.line,"terminus":t.terminus,"train_reverse":t.train_reverse,
                "stations":t.stations.iter().map(ToString::to_string).collect::<Vec<_>>(),"stations_legacy":t.stations_legacy,
                "profiles":t.profiles.iter().map(|p|json!({"name":p.name,"factor":p.factor,
                    "manual_arrival_times":p.man_arr_time,"manual_departure_times":p.man_dep_time,
                    "other_stopping":p.other_stopping})).collect::<Vec<_>>()}),
            )
        }
        "timetable.stops" => page(&data.bus_stops, args, |i, s| {
            json!({"index":i,"name":s.name,"group":s.group,
            "object_id":s.object_id.to_string(),"parameters":s.params})
        }),
        "timetable.links" => page(&data.stn_links, args, |i, l| {
            json!({"index":i,"length":l.length,
            "from_id":l.from_id.to_string(),"to_id":l.to_id.to_string(),"parameters":l.params,"entry_count":l.entries.len()})
        }),
        "timetable.link" => {
            let i = index(args, "index")?;
            let l = data.stn_links.get(i).ok_or("link index is out of range")?;
            Ok(
                json!({"index":i,"length":l.length,"from_id":l.from_id.to_string(),"to_id":l.to_id.to_string(),
                "parameters":l.params,"entries":l.entries.iter().map(|e|e.values).collect::<Vec<_>>()}),
            )
        }
        "timetable.tracks" => page(
            &data.tracks,
            args,
            |i, t| json!({"index":i,"path":t.path.to_string_lossy(),"entry_count":t.entries.len()}),
        ),
        "timetable.track" => {
            let i = index(args, "index")?;
            let t = data.tracks.get(i).ok_or("track index is out of range")?;
            Ok(
                json!({"index":i,"path":t.path.to_string_lossy(),"entries":t.entries.iter().map(|e|&e.values).collect::<Vec<_>>()}),
            )
        }
        _ => Err(format!("unsupported timetable API operation: {operation}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_pages_preserve_original_indices_and_reject_invalid_bounds() {
        let values = ["one", "two", "three"];
        let result = page(
            &values,
            &json!({"offset":1,"limit":1}),
            |i, s| json!({"index":i,"value":s}),
        )
        .unwrap();
        assert_eq!(result["items"][0]["index"], 1);
        assert_eq!(result["next_offset"], 2);
        assert!(page(&values, &json!({"offset":4}), |_, _| Value::Null).is_err());
        assert!(page(&values, &json!({"limit":0}), |_, _| Value::Null).is_err());
        assert!(page(&values, &json!({"limit":257}), |_, _| Value::Null).is_err());
    }

    #[test]
    fn date_preflight_failure_keeps_clock_and_duty_unchanged() {
        use clap::Parser;
        let root = std::env::temp_dir().join(format!(
            "openomsi-date-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("TTData")).unwrap();
        std::fs::write(root.join("global.cfg"), "[name]\nNative API fixture\n").unwrap();
        std::fs::write(
            root.join("TTData/Line.ttl"),
            "[newtour]\nTour\nGroup\n1023\n",
        )
        .unwrap();
        let mut app = crate::new_app(
            crate::Args::parse_from(["openomsi", "--root", root.to_str().unwrap()]),
            crate::settings::Settings::default(),
        );
        let world = std::sync::Arc::new(
            crate::scene::World::open(&root, &root.join("global.cfg"), app.clock.date_code())
                .unwrap(),
        );
        app.schedule = Some(crate::schedule::Schedule::new(&root, &world, &app.clock));
        app.world = Some(world);
        let before = (app.clock.year, app.clock.day_of_year, app.clock.time);
        let update = crate::native_bridge::protocol::ClockUpdate {
            year: Some(app.clock.year + 1),
            ..Default::default()
        };
        assert!(set_clock(&mut app, &update)
            .unwrap_err()
            .contains("renderer"));
        assert_eq!(
            before,
            (app.clock.year, app.clock.day_of_year, app.clock.time)
        );
        assert!(app.duty.is_none());
        let schedule = app.schedule.as_ref().unwrap();
        assert!(schedule.require_tour("Line", "Tour").is_ok());
        assert!(schedule.require_tour("Line", "nonexistent").is_err());
        drop(app);
        let path = root.canonicalize().unwrap();
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        assert!(path.starts_with(&temporary) && path != temporary);
        std::fs::remove_dir_all(path).unwrap();
    }
}
