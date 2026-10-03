//! Vehicle sound definitions and mixer-backed playback, after central identity checks.
use crate::App;
use omsi_audio::{AudioEngine, SoundSet};
use omsi_vehicle::sound::Condition;
use omsi_vehicle::{SoundEntry, VolCurve};
use serde_json::{json, Value};

fn keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    for key in args
        .as_object()
        .ok_or("arguments must be an object")?
        .keys()
    {
        if ![
            "id",
            "vehicle_id",
            "generation",
            "session_id",
            "section",
            "audio_generation",
        ]
        .contains(&key.as_str())
            && !allowed.contains(&key.as_str())
        {
            return Err(format!("unsupported audio argument: {key}"));
        }
    }
    Ok(())
}
fn index(args: &Value, key: &str, default: Option<usize>) -> Result<usize, String> {
    match args.get(key) {
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("{key} must be a nonnegative integer")),
        None => default.ok_or_else(|| format!("{key} is required")),
    }
}
fn number(value: &Value, name: &str, min: f32, max: f32) -> Result<f32, String> {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= min as f64 && *v <= max as f64)
        .map(|v| v as f32)
        .ok_or_else(|| format!("{name} must be between {min} and {max}"))
}
fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(|| format!("{name} must be a nonempty string of at most 256 bytes"))
}
fn vector(value: &Value, name: &str) -> Result<Option<[f32; 3]>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let a = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| format!("{name} must be a three-number array or null"))?;
    Ok(Some([
        number(&a[0], name, -1e6, 1e6)?,
        number(&a[1], name, -1e6, 1e6)?,
        number(&a[2], name, -1e6, 1e6)?,
    ]))
}

fn patch(
    def: &SoundEntry,
    pitch: f32,
    values: &Value,
    has_variable: &dyn Fn(&str) -> bool,
) -> Result<(SoundEntry, f32), String> {
    let values = values
        .as_object()
        .filter(|v| !v.is_empty())
        .ok_or("values must be a nonempty object")?;
    let mut result = def.clone();
    let mut pitch = pitch;
    let variable = |v: &Value| -> Result<String, String> {
        let name = text(v, "variable")?;
        if !has_variable(name) {
            return Err(format!("unknown sound variable: {name}"));
        }
        Ok(name.into())
    };
    for (key, value) in values {
        match key.as_str() {
            "file_name" => result.file = text(value, key)?.into(),
            "volume" => result.volume = number(value, key, 0.0, 16.0)?,
            "pitch_multiplier" => pitch = number(value, key, 0.001, 64.0)?,
            "sample_rate" => result.sample_rate = number(value, key, 100.0, 384000.0)?,
            "pitch_reference" => result.pitch_ref = number(value, key, 0.0001, 1e9)?,
            "pitch_variable" => {
                result.pitch_variable = if value.as_str() == Some("") {
                    String::new()
                } else {
                    variable(value)?
                }
            }
            "loop_sound" => result.is_loop = value.as_bool().ok_or("loop_sound must be boolean")?,
            "no_loop" => result.no_loop = value.as_bool().ok_or("no_loop must be boolean")?,
            "only_one" => result.only_one = value.as_bool().ok_or("only_one must be boolean")?,
            "position_local" => result.pos = vector(value, key)?,
            "direction_local" => result.dir = vector(value, key)?,
            "range_metres" => result.range = number(value, key, 0.0, 1e6)?,
            "viewpoint" => {
                result.viewpoint = value
                    .as_u64()
                    .filter(|v| *v <= 7)
                    .ok_or("viewpoint must be an integer in 0..7")?
                    as i32
            }
            "triggers" => {
                let names = value
                    .as_array()
                    .filter(|v| v.len() <= 64)
                    .ok_or("triggers must contain at most 64 names")?;
                let mut seen = std::collections::BTreeSet::new();
                result.triggers = names
                    .iter()
                    .map(|v| {
                        let name = text(v, "trigger")?;
                        if !seen.insert(name.to_ascii_lowercase()) {
                            return Err("duplicate trigger name".into());
                        }
                        Ok(name.to_string())
                    })
                    .collect::<Result<_, String>>()?;
            }
            "conditions" => {
                let conditions = value
                    .as_array()
                    .filter(|v| v.len() <= 64)
                    .ok_or("conditions must contain at most 64 records")?;
                result.conditions = conditions
                    .iter()
                    .map(|c| {
                        exact_object(c, &["variable", "relation", "value"])?;
                        Ok(Condition {
                            variable: variable(&c["variable"])?,
                            relation: c["relation"]
                                .as_u64()
                                .filter(|n| *n <= 5)
                                .ok_or("condition relation must be an integer in 0..5")?
                                as i32,
                            value: number(&c["value"], "condition value", -1e9, 1e9)?,
                        })
                    })
                    .collect::<Result<_, String>>()?;
            }
            "volume_curves" => {
                let curves = value
                    .as_array()
                    .filter(|v| v.len() <= 64)
                    .ok_or("volume_curves must contain at most 64 records")?;
                result.vol_curves = curves
                    .iter()
                    .map(|c| {
                        exact_object(c, &["variable", "points"])?;
                        let name = text(&c["variable"], "curve variable")?;
                        if !["-1", "-2"].contains(&name) && !has_variable(name) {
                            return Err(format!("unknown curve variable: {name}"));
                        }
                        let points = c["points"]
                            .as_array()
                            .filter(|v| !v.is_empty() && v.len() <= 128)
                            .ok_or("curve points require 1..128 pairs")?;
                        let points = points
                            .iter()
                            .map(|p| {
                                let p = p
                                    .as_array()
                                    .filter(|p| p.len() == 2)
                                    .ok_or("curve point must be a pair")?;
                                Ok((
                                    number(&p[0], "curve x", -1e9, 1e9)?,
                                    number(&p[1], "curve y", 0.0, 16.0)?,
                                ))
                            })
                            .collect::<Result<Vec<_>, String>>()?;
                        if points.windows(2).any(|p| p[0].0 >= p[1].0) {
                            return Err("curve x coordinates must strictly increase".into());
                        }
                        Ok(VolCurve {
                            variable: name.into(),
                            points,
                        })
                    })
                    .collect::<Result<_, String>>()?;
            }
            _ => return Err(format!("unsupported sound property: {key}")),
        }
    }
    Ok((result, pitch))
}
fn exact_object(value: &Value, allowed: &[&str]) -> Result<(), String> {
    if value
        .as_object()
        .ok_or("expected a property object")?
        .keys()
        .any(|k| !allowed.contains(&k.as_str()))
    {
        return Err("unsupported property in audio record".into());
    }
    Ok(())
}

fn definition(def: &SoundEntry) -> Value {
    json!({"file_name":def.file,"volume":def.volume,"loop_sound":def.is_loop,"sample_rate":def.sample_rate,
        "pitch_variable":def.pitch_variable,"pitch_reference":def.pitch_ref,"position_local":def.pos,
        "range_metres":def.range,"direction_local":def.dir,"no_loop":def.no_loop,"only_one":def.only_one,"viewpoint":def.viewpoint,
        "triggers":def.triggers,"conditions":def.conditions.iter().map(|c|json!({"variable":c.variable,"relation":c.relation,"value":c.value})).collect::<Vec<_>>(),
        "volume_curves":def.vol_curves.iter().map(|c|json!({"variable":c.variable,"points":c.points})).collect::<Vec<_>>(),
        "authored_metadata":{"important":def.important,"check_loading":def.check_loading,"random":def.random}})
}
fn record(
    sounds: &SoundSet,
    engine: &AudioEngine,
    index: usize,
    full: bool,
) -> Result<Value, String> {
    let s = sounds
        .entry(index, engine)
        .ok_or("sound index is unavailable")?;
    let mut result = json!({"index":index,"file_name":s.definition.file,"loaded":s.clip.is_some(),"playing":s.voice.is_some(),
        "playback_control":s.control.name(),"pitch_multiplier":s.pitch_multiplier,"active_seconds":s.active_seconds});
    if full {
        result["definition"] = definition(s.definition);
        result["clip"] = s
            .clip
            .map(|c| json!({"sample_rate":c.sample_rate,"channels":c.channels,"frames":c.frames()}))
            .unwrap_or(Value::Null);
        result["voice"]=s.voice.map(|(p,heard)|json!({"gain":p.gain,"gain_at_listener":heard,"pitch":p.pitch,"looping":p.looping,
            "world_position":p.position.map(|p|p.to_array()),"range_metres":p.range,"lowpass_hz":p.lowpass_hz,"doppler":p.doppler})).unwrap_or(Value::Null);
    }
    Ok(result)
}

pub(crate) fn execute(
    app: &mut App,
    id: u64,
    operation: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if !operation.starts_with("audio.") {
        return None;
    }
    Some((|| {
        let allowed: &[&str] = match operation {
            "audio.list" => &["offset", "limit"],
            "audio.get" | "audio.stop" | "audio.reset" => &["index"],
            "audio.set" => &["index", "values"],
            "audio.play" => &["index", "looping"],
            "audio.trigger" => &["name"],
            _ => return Err(format!("unsupported audio operation: {operation}")),
        };
        keys(args, allowed)?;
        let writing = !matches!(operation, "audio.list" | "audio.get");
        if writing && !args.get("session_id").is_some_and(Value::is_string) {
            return Err("session_id is required for audio writes".into());
        }
        let section = index(args, "section", Some(0))?;
        let engine = app.audio.as_ref().ok_or("audio engine is unavailable")?;
        let player = app
            .player
            .iter_mut()
            .chain(app.placed.iter_mut())
            .find(|p| p.uid == id)
            .ok_or("vehicle is no longer loaded")?;
        let vehicle = &mut player.vehicle;
        let transform = if section == 0 {
            vehicle.world_transform()
        } else {
            vehicle
                .trailers
                .get(section - 1)
                .ok_or("vehicle section is unavailable")?
                .world_transform()
        };
        let sounds = player
            .sounds
            .as_mut()
            .ok_or("vehicle sound set is not loaded")?;
        let sounds = if section == 0 {
            sounds
        } else {
            &mut sounds
                .parts
                .iter_mut()
                .find(|(part, _)| *part == section - 1)
                .ok_or("vehicle section has no loaded sound set")?
                .1
        };
        if let Some(g) = args.get("audio_generation") {
            if g.as_str().and_then(|s| s.parse::<u64>().ok()) != Some(sounds.native_id()) {
                return Err("stale audio generation; enumerate sounds again".into());
            }
        } else if writing {
            return Err("audio_generation from audio.list is required for writes".into());
        }
        if operation == "audio.list" {
            let offset = index(args, "offset", Some(0))?;
            let limit = index(args, "limit", Some(64))?;
            if !(1..=128).contains(&limit) || offset > sounds.len() {
                return Err("invalid sound page (limit 1..128)".into());
            }
            let end = offset.saturating_add(limit).min(sounds.len());
            return Ok(
                json!({"audio_generation":sounds.native_id().to_string(),"section":section,"output_enabled":engine.enabled,
                "total":sounds.len(),"next_offset":if end<sounds.len(){Some(end)}else{None},
                "items":(offset..end).map(|i|record(sounds,engine,i,false)).collect::<Result<Vec<_>,_>>()?}),
            );
        }
        if operation == "audio.trigger" {
            if !engine.enabled {
                return Err("audio output is unavailable; no trigger was queued".into());
            }
            let name = text(&args["name"], "name")?;
            if !sounds.has_trigger(name) {
                return Err("sound trigger is not declared by this section".into());
            }
            if vehicle.host.fired_triggers.len() >= 512 {
                return Err("sound trigger queue is full".into());
            }
            vehicle.host.fired_triggers.push(name.into());
            return Ok(json!({"queued":true,"name":name}));
        }
        let index = index(args, "index", None)?;
        if operation == "audio.get" {
            let mut result = record(sounds, engine, index, true)?;
            result["audio_generation"] = json!(sounds.native_id().to_string());
            result["output_enabled"] = json!(engine.enabled);
            return Ok(result);
        }
        match operation {
            "audio.set" => {
                let current = sounds
                    .entry(index, engine)
                    .ok_or("sound index is unavailable")?;
                let (next, pitch) = patch(
                    current.definition,
                    current.pitch_multiplier,
                    &args["values"],
                    &|n| vehicle.var(n).is_some(),
                )?;
                if next.file != current.definition.file && next.file.trim().parse::<i32>().is_err()
                {
                    let path = omsi_cfg::resolve_path(sounds.directory(), &next.file)
                        .canonicalize()
                        .map_err(|e| e.to_string())?;
                    let root = app.args.root.canonicalize().map_err(|e| e.to_string())?;
                    if !path.starts_with(&root) {
                        return Err("sound file resolves outside the content root".into());
                    }
                }
                sounds.replace_entry(engine, index, next, pitch)?;
            }
            "audio.play" => {
                if !engine.enabled {
                    return Err("audio output is unavailable; no playback was started".into());
                }
                let looping = args
                    .get("looping")
                    .map(|v| v.as_bool().ok_or("looping must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                sounds.play_entry(engine, index, looping, &|n| vehicle.var(n), &transform)?;
            }
            "audio.stop" => sounds.stop_entry(engine, index)?,
            "audio.reset" => sounds.reset_entry(engine, index)?,
            _ => unreachable!(),
        }
        Ok(
            json!({"index":index,"section":section,"audio_generation":sounds.native_id().to_string(),"queued":operation=="audio.set", "output_enabled":engine.enabled}),
        )
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn definition_batch_rejects_unknown_and_invalid_without_changing_original() {
        let original = SoundEntry {
            volume: 0.5,
            ..Default::default()
        };
        for values in [
            json!({"volume":0.8,"unknown":1}),
            json!({"volume":0.8,"pitch_variable":"missing"}),
            json!({"volume_curves":[{"variable":"rpm","points":[[1,1],[0,0]]}]}),
            json!({"loop_sound":1}),
        ] {
            assert!(patch(&original, 1.0, &values, &|n| n == "rpm").is_err());
            assert_eq!(original.volume, 0.5);
        }
        let (updated, pitch) = patch(
            &original,
            1.0,
            &json!({"volume":0.8,"pitch_multiplier":2.0,
            "conditions":[{"variable":"rpm","relation":4,"value":2.5}],
            "volume_curves":[{"variable":"rpm","points":[[0,0],[1,1]]}]}),
            &|n| n == "rpm",
        )
        .unwrap();
        assert_eq!((updated.volume, pitch), (0.8, 2.0));
        assert_eq!(updated.vol_curves.len(), 1);
        assert_eq!(updated.conditions[0].variable, "rpm");
        assert_eq!(updated.conditions[0].relation, 4);
        assert_eq!(updated.conditions[0].value, 2.5);
        assert!(updated.conditions[0].holds(2.5));
        assert!(!updated.conditions[0].holds(3.0));
    }
}
