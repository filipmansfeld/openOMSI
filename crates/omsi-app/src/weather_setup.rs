//! The weather of a session: its `.owt` file, the sky and clouds it makes, and how wet the streets are.

use super::*;

/// Weather from `--weather`, else the clear-sky default.
pub(crate) fn load_weather(args: &Args) -> omsi_content::weather::Weather {
    let rel = args
        .weather
        .clone()
        .filter(|w| !crate::weather_cycle::is_cycle(Some(w)))
        .unwrap_or_else(|| "Weather/#CAVOK.owt".into());
    // OMSI 2's current weather: `metar:<ICAO>` fetches the airport's report
    let loaded = match rel.strip_prefix("metar:").or_else(|| rel.strip_prefix("METAR:")) {
        Some(icao) => Ok(fetch_metar(icao.trim())),
        None => omsi_content::weather::Weather::load(&omsi_cfg::resolve_path(&args.root, &rel)),
    };
    match loaded {
        Ok(w) => {
            log::info!(
                "weather {}: fog range {} m, precip {:?}, temp {:?}",
                w.name,
                w.fog.0,
                w.precip,
                w.temp
            );
            let mut w = w;
            // No weather chosen: the clear default (#CAVOK, no cloud at all) gets a few fair-
            // weather cumulus clouds, as long as clouds are wanted - a sky without a single
            // cloud was the first thing that looked wrong.
            if args.weather.is_none() && CLOUDS.load(std::sync::atomic::Ordering::Relaxed) && w.clouds.0.trim().starts_with("-1") {
                w.clouds = ("Cumulus 1".into(), 100.0);
            }
            // the vehicles ask for it while they go onto the GPU (the snow on the panes)
            scene::SNOW_WEATHER.store(w.snow, std::sync::atomic::Ordering::Relaxed);
            // and their {init} reads the temperature
            omsi_sim::host::set_ambient_weather(w.temp.0, w.temp.1);
            w
        }
        Err(e) => {
            log::warn!("weather {rel}: {e}");
            scene::SNOW_WEATHER.store(false, std::sync::atomic::Ordering::Relaxed);
            omsi_content::weather::Weather {
                fog: (50000.0, 1.0),
                ..Default::default()
            }
        }
    }
}

/// Sky gradient textures from envir.cfg uploaded into the scene.
pub(crate) fn setup_sky(
    args: &Args,
    renderer: &Renderer,
    scene: &mut Scene,
    envir: Option<&omsi_content::Envir>,
    weather: Option<&omsi_content::weather::Weather>,
) {
    let names = envir.map(|e| e.sky_textures.clone()).unwrap_or_else(|| {
        [
            "Texture\\himmel01.bmp".into(),
            "Texture\\himmel04.bmp".into(),
            "Texture\\himmel05.bmp".into(),
        ]
    });
    let mut ids = Vec::new();
    for n in &names {
        let p = omsi_cfg::resolve_path(&args.root, n);
        match omsi_texture::decode_file(&p) {
            Ok(img) => ids.push(renderer.add_texture(scene, &img, false)),
            Err(e) => {
                log::warn!("sky texture {}: {e}", p.display());
                return;
            }
        }
    }
    // the weather's cloud type (Weather/clouds.cfg: Cumulus 1..3, Overcast 1) with its own
    // texture, whose alpha is the clouds' shape; `Texture\clouds.tga` when there is none
    let kind = weather.map(|w| w.clouds.0.trim().to_string()).unwrap_or_default();
    let typed = cloud_texture(&args.root, &kind);
    let cover = typed.or_else(|| omsi_texture::decode_file(&omsi_cfg::resolve_path(&args.root, "Texture\\clouds.tga")).ok());
    let t = std::time::Instant::now();
    let field = cloud_field(cover.as_ref());
    log::debug!("cloud field made in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let clouds = Some(renderer.add_texture(scene, &field, true));
    renderer.set_sky_textures_clouds(scene, [ids[0], ids[1], ids[2]], clouds);
}

/// Edge of the cloud field texture (texels); it tiles.
const FIELD: usize = 512;

/// The texture both sky shaders draw their clouds from, seamless in both directions:
/// R the weather's own cloud picture (Weather/clouds.cfg) - where its clouds are, the sky
/// has more; G the shape of the cumulus, fractal noise bent by more noise, equalised so
/// that a threshold of 1 - f covers exactly the fraction f of the sky; B billows for the
/// cauliflower edges; A how tall each cloud grows. Stored in sRGB bytes so that the
/// shader reads the values back as they were made.
pub(crate) fn cloud_field(cover: Option<&omsi_texture::Image>) -> omsi_texture::Image {
    let n = FIELD;
    let lattice = |x: i64, y: i64, period: i64, seed: u32| -> f32 {
        let (x, y) = (x.rem_euclid(period) as u32, y.rem_euclid(period) as u32);
        let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1_e995);
        h ^= h >> 15;
        (h & 0xffff) as f32 / 65535.0
    };
    // value noise with `period` cells across the tile, at a point of the tile (0..1)
    let noise = |u: f32, v: f32, period: i64, seed: u32| -> f32 {
        let (x, y) = (u * period as f32, v * period as f32);
        let (ix, iy) = (x.floor() as i64, y.floor() as i64);
        let (fx, fy) = (x - ix as f32, y - iy as f32);
        let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let a = lattice(ix, iy, period, seed) + (lattice(ix + 1, iy, period, seed) - lattice(ix, iy, period, seed)) * sx;
        let b = lattice(ix, iy + 1, period, seed) + (lattice(ix + 1, iy + 1, period, seed) - lattice(ix, iy + 1, period, seed)) * sx;
        a + (b - a) * sy
    };
    let fbm = |u: f32, v: f32, period: i64, octaves: u32, seed: u32| -> f32 {
        let (mut sum, mut amp, mut norm, mut p) = (0.0, 0.5, 0.0, period);
        for o in 0..octaves {
            sum += amp * noise(u, v, p, seed + o * 101);
            norm += amp;
            amp *= 0.5;
            p *= 2;
        }
        sum / norm
    };
    let mut shape = vec![0f32; n * n];
    let mut billow = vec![0f32; n * n];
    let mut tall = vec![0f32; n * n];
    // (rows in parallel: 512 x 512 texels of a few octaves each)
    use rayon::prelude::*;
    shape
        .par_chunks_mut(n)
        .zip(billow.par_chunks_mut(n))
        .zip(tall.par_chunks_mut(n))
        .enumerate()
        .for_each(|(y, ((s_row, b_row), t_row))| {
            let v = y as f32 / n as f32;
            for x in 0..n {
                let u = x as f32 / n as f32;
                // bend the cumulus field with a coarser one, so that clouds are not blobs on a grid
                let wu = fbm(u, v, 4, 3, 11) - 0.5;
                let wv = fbm(u, v, 4, 3, 23) - 0.5;
                s_row[x] = fbm(u + wu * 0.12, v + wv * 0.12, 6, 5, 37);
                let mut b = 0.0;
                let mut amp = 0.5;
                let mut p = 24;
                for o in 0..3 {
                    b += amp * (1.0 - (noise(u, v, p, 53 + o) * 2.0 - 1.0).abs());
                    amp *= 0.5;
                    p *= 2;
                }
                b_row[x] = b / 0.875;
                t_row[x] = fbm(u, v, 3, 2, 71);
            }
        });
    // equalise the shape: its rank, so that a threshold is a fraction of the sky
    let mut order: Vec<u32> = (0..(n * n) as u32).collect();
    order.par_sort_unstable_by(|a, b| shape[*a as usize].total_cmp(&shape[*b as usize]));
    let mut equal = vec![0f32; n * n];
    for (rank, &i) in order.iter().enumerate() {
        equal[i as usize] = rank as f32 / (n * n - 1) as f32;
    }
    let stretch = |v: &mut Vec<f32>| {
        let (lo, hi) = v.iter().fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        for x in v.iter_mut() {
            *x = (*x - lo) / (hi - lo).max(1e-6);
        }
    };
    stretch(&mut billow);
    stretch(&mut tall);
    let to_srgb = |x: f32| -> u8 {
        let x = x.clamp(0.0, 1.0);
        let s = if x <= 0.003_130_8 { x * 12.92 } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0 + 0.5) as u8
    };
    // the weather's picture, sampled bilinearly (it tiles as well), its brightness the cover
    let weather = |u: f32, v: f32| -> f32 {
        let Some(img) = cover else { return 0.5 };
        let (w, h) = (img.width as i64, img.height as i64);
        let (x, y) = (u * w as f32 - 0.5, v * h as f32 - 0.5);
        let (ix, iy) = (x.floor() as i64, y.floor() as i64);
        let (fx, fy) = (x - ix as f32, y - iy as f32);
        let px = |xx: i64, yy: i64| {
            let i = ((yy.rem_euclid(h) * w + xx.rem_euclid(w)) * 4) as usize;
            (img.rgba[i] as f32 + img.rgba[i + 1] as f32 + img.rgba[i + 2] as f32) / (3.0 * 255.0)
        };
        let a = px(ix, iy) + (px(ix + 1, iy) - px(ix, iy)) * fx;
        let b = px(ix, iy + 1) + (px(ix + 1, iy + 1) - px(ix, iy + 1)) * fx;
        a + (b - a) * fy
    };
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let i = y * n + x;
            let o = i * 4;
            rgba[o] = to_srgb(weather(x as f32 / n as f32, y as f32 / n as f32));
            rgba[o + 1] = to_srgb(equal[i]);
            rgba[o + 2] = to_srgb(billow[i]);
            // (alpha is not sRGB-coded)
            rgba[o + 3] = (tall[i].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }
    omsi_texture::Image { width: n as u32, height: n as u32, rgba, has_alpha: true }
}

/// The texture of a cloud type named in `Weather/clouds.cfg` (`[cloudtype]` name, texture,
/// size in metres, sct/ovc), as the sky shaders read it: the cover in the colour channels.
/// A scattered type's texture is white with the clouds in its alpha; an overcast one is a
/// picture of the cloud deck, its brightness is the cover.
fn cloud_texture(root: &Path, kind: &str) -> Option<omsi_texture::Image> {
    if kind.is_empty() || kind.starts_with("-1") {
        return None;
    }
    let cfg = omsi_cfg::vfs::read(&root.join("Weather").join("clouds.cfg")).ok()?;
    let text = omsi_cfg::codepage::decode(&cfg);
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    let mut file = None;
    for i in 0..lines.len() {
        if lines[i].eq_ignore_ascii_case("[cloudtype]") && lines.get(i + 1).map(|n| n.eq_ignore_ascii_case(kind)).unwrap_or(false) {
            file = lines.get(i + 2).map(|s| s.to_string());
            break;
        }
    }
    let file = file?;
    let mut img = omsi_texture::decode_file(&omsi_cfg::resolve_path(&omsi_cfg::resolve_path(&root, "Texture"), &file)).ok()?;
    if img.has_alpha {
        for px in img.rgba.chunks_mut(4) {
            let a = px[3];
            px[0] = a;
            px[1] = a;
            px[2] = a;
            px[3] = 255;
        }
        img.has_alpha = false;
    }
    log::info!("clouds: {kind} ({file})");
    Some(img)
}

/// Cloud cover of a weather file: `[clouds] type density`, type -1 = clear, density up to
/// ~300 (Cumulus 3) - mapped to 0..1; the cover drifts with the wind.
pub(crate) fn clouds_of(w: &omsi_content::weather::Weather, time: f64) -> (f32, [f32; 2]) {
    let kind = w.clouds.0.trim();
    if kind.is_empty() || kind.starts_with("-1") || !CLOUDS.load(std::sync::atomic::Ordering::Relaxed) {
        return (0.0, [0.0; 2]);
    }
    // cover by type: Cumulus 1..3 scattered → broken, Overcast closed
    let lower = kind.to_ascii_lowercase();
    let density = if lower.starts_with("overcast") {
        1.0
    } else if lower.starts_with("cumulus") {
        match lower
            .trim_start_matches("cumulus")
            .trim()
            .parse::<i32>()
            .unwrap_or(1)
        {
            1 => 0.35,
            2 => 0.55,
            _ => 0.75,
        }
    } else {
        0.5
    };
    // [wind] direction (deg) speed (m/s): drift over the 2500 m tiling. (It was the time of
    // day, and at midnight the whole sky jumped back.)
    let (dir, speed) = (w.wind.0.to_radians(), w.wind.1);
    // `time` runs on over midnight (seconds since the year began, see the callers); the
    // field repeats every tile, so only the fraction of a tile is kept
    let drift = (time * speed as f64 / 2500.0).rem_euclid(1.0) as f32;
    (density, [dir.sin() * drift, dir.cos() * drift])
}

/// How wet the roads are: rain soaks them in a few minutes, sunshine dries them in about
/// twenty. `secs` is how long this weather has been running.
pub(crate) fn road_wetness(rate: f32, secs: f64, start: f32) -> f32 {
    if rate > 0.0 {
        (start + secs as f32 * rate / 180.0).clamp(0.0, 1.0)
    } else {
        (start - secs as f32 / 1200.0).clamp(0.0, 1.0)
    }
}

/// The renderer's lighting for this weather at this moment: the daylight, then what the
/// cloud cover, the rain and the snow make of it.
pub(crate) fn weather_lighting(
    daylight: &omsi_sim::Daylight,
    w: &omsi_content::weather::Weather,
    time: f64,
    wetness: f32,
    shadows: bool,
) -> omsi_render::Lighting {
    let mut lighting = lights::lighting_from(daylight, w.fog.0);
    lighting.enhanced = ENHANCED.load(std::sync::atomic::Ordering::Relaxed);
    lighting.classic = CLASSIC.load(std::sync::atomic::Ordering::Relaxed);
    let (density, offset) = clouds_of(w, time);
    lighting.cloud_density = density;
    lighting.cloud_offset = offset;
    let (kind, rate) = precip_of(w);
    lights::apply_weather(
        &mut lighting,
        density,
        kind,
        rate,
        if w.snow { 1.0 } else { 0.0 },
    );
    lighting.wetness = wetness;
    lighting.shadows = shadows;
    lighting
}

/// Push the weather into a vehicle's script host.
pub(crate) fn precip_of(w: &omsi_content::weather::Weather) -> (i32, f32) {
    let kind = w.precip.first().copied().unwrap_or(0.0) as i32;
    let rate = if kind == 0 {
        0.0
    } else {
        (w.precip.get(1).copied().unwrap_or(0.0) / 255.0).clamp(0.0, 1.0)
    };
    (kind, rate)
}

/// Small current snapshot used before AI `{init}` as well as on live instances.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VehicleWeather {
    precip_type: f32,
    precip_rate: f32,
    street_cond: f32,
    temperature: f32,
    abs_humidity: f32,
}
impl VehicleWeather {
    pub(crate) fn from_weather(weather: &omsi_content::weather::Weather, wetness: f32) -> Self {
        let (kind, rate) = precip_of(weather);
        Self {
            precip_type: kind as f32,
            precip_rate: rate,
            street_cond: street_condition(weather, wetness),
            temperature: weather.temp.0,
            abs_humidity: weather.temp.1,
        }
    }

    pub(crate) fn apply_host(self, host: &mut omsi_sim::VehicleHost) {
        host.precip_type = self.precip_type;
        host.precip_rate = self.precip_rate;
        host.street_cond = self.street_cond;
        host.temperature = self.temperature;
        host.abs_humidity = self.abs_humidity;
    }

    pub(crate) fn apply_vehicle(self, vehicle: &mut omsi_sim::VehicleInstance) {
        self.apply_host(&mut vehicle.host);
        vehicle.set_var("PrecipType", self.precip_type);
        vehicle.set_var("PrecipRate", self.precip_rate);
        vehicle.set_var("StreetCond", self.street_cond);
    }
}

pub(crate) fn apply_weather(
    v: &mut omsi_sim::VehicleInstance,
    w: &omsi_content::weather::Weather,
    wetness: f32,
) {
    VehicleWeather::from_weather(w, wetness).apply_vehicle(v);
    omsi_sim::host::set_ambient_weather(w.temp.0, w.temp.1);
}

/// `OMSI_DEBUG_SOUND[=seconds]`: how often the mixer says what every sound entry of the
/// player's bus is doing, and what the environment sounds play (5 s by default).
pub(crate) fn debug_sound_every() -> Option<f32> {
    static EVERY: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        omsi_cfg::env::var("OMSI_DEBUG_SOUND")
            .ok()
            .map(|v| v.parse::<f32>().ok().filter(|s| *s > 0.0).unwrap_or(5.0))
    })
}

/// The road under the wheels as the scripts and the sound configurations read it
/// (`StreetCond`): 0 dry, 1 wet, 2 covered in snow. A snowfall (or a weather with
/// `[snowOnRoad]`) puts it into the upper half, everything else follows the water film.
pub(crate) fn street_condition(w: &omsi_content::weather::Weather, wetness: f32) -> f32 {
    let wet = wetness.clamp(0.0, 1.0);
    if w.snow_on_road || precip_of(w).0 == 2 {
        1.0 + wet
    } else {
        wet
    }
}

/// How wet the roads are when a session starts: what this weather has already left on them
/// (`[groundwet]`, 0 … 255), and at least what the rain falling now would soak them to.
pub(crate) fn initial_wetness(w: &omsi_content::weather::Weather) -> f32 {
    let rate = precip_of(w).1;
    let falling = if rate > 0.0 {
        (0.4 + rate).min(1.0)
    } else {
        0.0
    };
    (w.ground_wet[0] / 255.0).clamp(0.0, 1.0).max(falling)
}


/// The weather of an airport's METAR report (aviationweather.gov), or a clear day when it
/// cannot be had (no network, an unknown station).
pub(crate) fn fetch_metar(icao: &str) -> omsi_content::weather::Weather {
    // (Tegel, OMSI's Berlin default, closed in 2020: Berlin's airport now reports)
    let icao = match icao.to_ascii_uppercase().as_str() {
        "EDDT" | "EDDI" | "" => "EDDB".to_string(),
        other => other.to_string(),
    };
    let url = format!("https://aviationweather.gov/api/data/metar?ids={icao}&format=raw");
    let text = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(6))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok())
        .map(|t| t.lines().next().unwrap_or("").trim().to_string())
        .filter(|t| !t.is_empty());
    match text {
        Some(t) => {
            log::info!("current weather at {icao}: {t}");
            let mut w = omsi_content::weather::from_metar(&icao, &t);
            w.path = std::path::PathBuf::from(format!("metar:{icao}"));
            w
        }
        None => {
            log::warn!("current weather at {icao}: no METAR report could be had; a clear day instead");
            omsi_content::weather::from_metar(&icao, "CAVOK")
        }
    }
}
