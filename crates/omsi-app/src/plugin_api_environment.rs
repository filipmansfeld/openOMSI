//! Native active weather. Updates are validated before touching the simulation.
use crate::App;
use omsi_content::weather::Weather;
use serde_json::{json, Value};

fn finite(value: &Value, name: &str, lo: f32, hi: f32) -> Result<f32, String> {
    let n = value.as_f64().ok_or_else(||format!("{name} must be a number"))?;
    if !n.is_finite() || n < lo as f64 || n > hi as f64 { return Err(format!("{name} must be in {lo}..{hi}")); }
    Ok(n as f32)
}

fn update(current: &Weather, wetness: f32, args: &Value) -> Result<(Weather, f32), String> {
    let values = args.get("values").and_then(Value::as_object).filter(|v|!v.is_empty()).ok_or("values must be a nonempty object")?;
    for key in args.as_object().ok_or("arguments must be an object")?.keys() {
        if !["session_id", "values"].contains(&key.as_str()) { return Err(format!("unsupported argument: {key}")); }
    }
    let mut w = current.clone();
    let mut wet = wetness;
    for (key, value) in values {
        match key.as_str() {
            "visibility_metres" => w.fog.0 = finite(value,key,1.0,1_000_000.0)?,
            "wind_direction_degrees" => w.wind.0 = finite(value,key,-360_000.0,360_000.0)?.rem_euclid(360.0),
            "wind_metres_per_second" => w.wind.1 = finite(value,key,0.0,200.0)?,
            "temperature_celsius" => w.temp.0 = finite(value,key,-150.0,150.0)?,
            "absolute_humidity_grams_per_cubic_metre" => w.temp.1 = finite(value,key,0.0,1000.0)?,
            "road_wetness" => wet = finite(value,key,0.0,1.0)?,
            "cloud_type" => {
                let s = value.as_str().ok_or("cloud_type must be a string")?;
                // These are the active categories interpreted by clouds_of. Arbitrary
                // labels would produce a sky with an unrelated fallback texture.
                if !["-1","Cumulus 1","Cumulus 2","Cumulus 3","Overcast 1"].contains(&s) {
                    return Err("cloud_type must be -1, Cumulus 1/2/3 or Overcast 1".into());
                }
                w.clouds.0 = s.to_owned();
            }
            "precipitation_kind" => {
                let kind = value.as_u64().filter(|n|*n<=2).ok_or("precipitation_kind must be 0 (none), 1 (rain), or 2 (snow)")?;
                w.precip.resize(w.precip.len().max(5),0.0);
                w.precip[0] = kind as f32;
            }
            "precipitation_rate" => {
                let rate = finite(value,key,0.0,1.0)?;
                w.precip.resize(w.precip.len().max(5),0.0);
                w.precip[1] = rate*255.0;
            }
            "snow" => w.snow = value.as_bool().ok_or("snow must be a boolean")?,
            "snow_on_road" => w.snow_on_road = value.as_bool().ok_or("snow_on_road must be a boolean")?,
            _ => return Err(format!("unsupported active weather field: {key}")),
        }
    }
    Ok((w,wet))
}

fn snapshot(w: &Weather, wetness: f32) -> Value {
    let (kind,rate) = crate::weather_setup::precip_of(w);
    json!({"name":w.name,"description":w.description,"source_file":w.path.to_string_lossy(),
        "visibility_metres":w.fog.0,"wind_direction_degrees":w.wind.0,"wind_metres_per_second":w.wind.1,
        "temperature_celsius":w.temp.0,"absolute_humidity_grams_per_cubic_metre":w.temp.1,
        "relative_humidity":omsi_sim::vehicle::relative_humidity(w.temp.0,w.temp.1),
        "cloud_type":w.clouds.0,"precipitation_kind":kind,"precipitation_rate":rate,
        "snow":w.snow,"snow_on_road":w.snow_on_road,"road_wetness":wetness,
        "street_condition":crate::weather_setup::street_condition(w,wetness),
        // Retained file metadata is not advertised as a simulated writable property.
        "preset_metadata":{"fog_density":w.fog.1,"pressure":w.pressure,"cloud_height":w.clouds.1,
            "precipitation":w.precip,"ground_wetness":w.ground_wet}})
}

pub(crate) fn execute(app: &mut App, operation: &str, args: &Value) -> Option<Result<Value,String>> {
    if !matches!(operation,"weather.get"|"weather.set"|"weather.presets"|"weather.select") { return None; }
    Some((|| {
        if operation == "weather.presets" {
            return Ok(json!(crate::weather_cycle::installed().iter().map(|(file,w)|
                json!({"file_name":file,"weather":snapshot(w,crate::weather_setup::initial_wetness(w))})).collect::<Vec<_>>()));
        }
        let current = app.weather.as_ref().ok_or("weather is not loaded")?;
        if operation == "weather.get" {
            let mut result = snapshot(current,app.wetness);
            result["transitioning"] = json!(app.weather_blend.is_some());
            result["cycling"] = json!(app.weather_cycle.is_some());
            return Ok(result);
        }
        if app.lan.is_some() { return Err("direct native weather changes currently require a local session".into()); }
        let (next,wetness) = if operation == "weather.select" {
            let file = args.get("file_name").and_then(Value::as_str).ok_or("file_name from weather.presets is required")?;
            for key in args.as_object().ok_or("arguments must be an object")?.keys() {
                if !["session_id","file_name"].contains(&key.as_str()) { return Err(format!("unsupported argument: {key}")); }
            }
            let w = crate::weather_cycle::installed().into_iter().find(|(f,_)|f==file).map(|(_,w)|w).ok_or("weather preset is not installed")?;
            let wet = crate::weather_setup::initial_wetness(&w);
            (w,wet)
        } else { update(current,app.wetness,args)? };
        let clouds_changed = current.clouds.0 != next.clouds.0;
        // The API takes ownership of the active conditions, ending pending blends and
        // the cycle so they cannot overwrite a successful write next frame.
        app.weather_blend = None;
        app.weather_cycle = None;
        app.wetness = wetness;
        crate::scene::SNOW_WEATHER.store(next.snow,std::sync::atomic::Ordering::Relaxed);
        omsi_sim::host::set_ambient_weather(next.temp.0,next.temp.1);
        for player in app.player.iter_mut().chain(app.placed.iter_mut()) {
            crate::weather_setup::apply_weather(&mut player.vehicle,&next,wetness);
        }
        if let Some(traffic) = app.traffic.as_mut() { traffic.set_weather(&next,wetness); }
        app.weather = Some(next);
        if clouds_changed {
            if let (Some(renderer),Some(scene)) = (app.renderer.as_ref(),app.scene.as_mut()) {
                crate::weather_setup::setup_sky(&app.args,renderer,scene,app.envir.as_ref(),app.weather.as_ref());
            }
        }
        Ok(snapshot(app.weather.as_ref().unwrap(),app.wetness))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changes_reach_the_native_vehicle_weather_fields() {
        let old=Weather{temp:(20.0,8.0),precip:vec![0.0;5],..Default::default()};
        let (next,wet)=update(&old,0.0,&json!({"values":{"temperature_celsius":-3,
            "precipitation_kind":2,"precipitation_rate":0.5,"road_wetness":0.75,"snow":true}})).unwrap();
        assert_eq!(crate::weather_setup::precip_of(&next),(2,0.5));
        assert_eq!(crate::weather_setup::street_condition(&next,wet),1.75);
        assert_eq!(next.temp,(-3.0,8.0));
        assert_eq!(old.temp,(20.0,8.0));
    }
    #[test]
    fn invalid_batch_cannot_change_current_weather() {
        let old=Weather{temp:(20.0,8.0),..Default::default()};
        for values in [json!({"temperature_celsius":0,"road_wetness":2}),
            json!({"temperature_celsius":0,"unknown":1}),json!({"precipitation_kind":1.5}),
            json!({"cloud_type":"made up"})] {
            assert!(update(&old,0.0,&json!({"values":values})).is_err());
            assert_eq!(old.temp,(20.0,8.0));
        }
    }
}
