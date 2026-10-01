//! Runtime controls; no JSON or process-memory representation leaks into the mixer.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(super) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackControl {
    Native,
    Stopped,
    Once,
    Loop,
}
impl PlaybackControl {
    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Stopped => "stopped",
            Self::Once => "once",
            Self::Loop => "loop",
        }
    }
}

pub struct SoundInfo<'a> {
    pub definition: &'a SoundEntry,
    pub clip: Option<&'a Clip>,
    pub voice: Option<(VoiceParams, f32)>,
    pub active_seconds: Option<f32>,
    pub control: PlaybackControl,
    pub pitch_multiplier: f32,
}

fn params(
    engine: &AudioEngine,
    s: &RuntimeSound,
    var: &dyn Fn(&str) -> Option<f32>,
    transform: &Mat4,
    master: f32,
    exterior: bool,
    muffled: bool,
    doppler: bool,
) -> VoiceParams {
    // Explicit playback bypasses automatic start conditions/view selection. Curves,
    // spatial attenuation and cabin muffling continue to act on the native voice.
    let active = s.active_since.map_or(0.0, |t| t.elapsed().as_secs_f32());
    let facing = match (s.def.pos, s.def.dir) {
        (Some(position), Some(direction)) => {
            let at = transform.transform_point3(Vec3::from_array(position));
            let direction = transform
                .transform_vector3(Vec3::from_array(direction))
                .normalize_or_zero();
            direction.dot((engine.listener_position() - at).normalize_or_zero())
        }
        _ => 1.0,
    };
    let mut gain = s.def.volume;
    for curve in &s.def.vol_curves {
        if let Some(x) = SoundSet::curve_input(curve, var, active, facing) {
            gain *= super::curve(&curve.points, x);
        }
    }
    let pitch = s
        .clip
        .as_ref()
        .map(|c| SoundSet::pitch_of(&s.def, var, c).0)
        .unwrap_or(1.0)
        * s.pitch_multiplier;
    VoiceParams {
        gain: gain.clamp(0.0, 1.0) * master * SoundSet::outside_gain(muffled, exterior),
        pitch: pitch.clamp(0.001, 64.0),
        looping: s.control == PlaybackControl::Loop,
        position: s
            .def
            .pos
            .map(|p| transform.transform_point3(Vec3::from_array(p)))
            .or_else(|| exterior.then(|| transform.transform_point3(Vec3::ZERO))),
        range: if s.def.range > 0.0 {
            s.def.range
        } else if exterior {
            40.0
        } else {
            5.0
        },
        lowpass_hz: SoundSet::lowpass_of(muffled, exterior),
        doppler,
        important: s.def.important,
    }
}

pub(super) fn update_controlled(
    engine: &AudioEngine,
    s: &mut RuntimeSound,
    var: &dyn Fn(&str) -> Option<f32>,
    transform: &Mat4,
    master: f32,
    exterior: bool,
    muffled: bool,
    doppler: bool,
) {
    if s.control == PlaybackControl::Stopped {
        if let Some(id) = s.voice.take() {
            engine.stop(id);
        }
        return;
    }
    let p = params(
        engine, s, var, transform, master, exterior, muffled, doppler,
    );
    if let Some(id) = s.voice {
        if engine.is_playing(id) {
            engine.set_params(id, p);
            return;
        }
        s.voice = None;
    }
    if s.control == PlaybackControl::Loop {
        if let Some(clip) = s.clip.clone() {
            s.voice = Some(engine.play(clip, p));
        }
    }
}

impl SoundSet {
    pub fn native_id(&self) -> u64 {
        self.api_id
    }
    pub fn directory(&self) -> &Path {
        &self.dir
    }
    pub fn entry(&self, index: usize, engine: &AudioEngine) -> Option<SoundInfo<'_>> {
        let s = self.sounds.get(index)?;
        Some(SoundInfo {
            definition: &s.def,
            clip: s.clip.as_deref(),
            voice: s.voice.and_then(|id| engine.voice_state(id)),
            active_seconds: s.active_since.map(|t| t.elapsed().as_secs_f32()),
            control: s.control,
            pitch_multiplier: s.pitch_multiplier,
        })
    }
    pub fn has_trigger(&self, name: &str) -> bool {
        self.sounds
            .iter()
            .any(|s| s.def.triggers.iter().any(|n| n.eq_ignore_ascii_case(name)))
    }

    /// The caller validates the complete definition. Decode a replacement clip
    /// before changing the live entry so a bad file cannot erase its old playback.
    pub fn replace_entry(
        &mut self,
        engine: &AudioEngine,
        index: usize,
        definition: SoundEntry,
        pitch_multiplier: f32,
    ) -> Result<(), String> {
        let s = self.sounds.get(index).ok_or("sound entry is unavailable")?;
        let changed_file = s.def.file != definition.file;
        let replacement = if changed_file {
            if definition.file.trim().parse::<i32>().is_ok() {
                Some(None)
            } else {
                Some(Some(
                    engine
                        .load_clip(&omsi_cfg::resolve_path(&self.dir, &definition.file))
                        .ok_or("sound file could not be decoded")?,
                ))
            }
        } else {
            None
        };
        let s = &mut self.sounds[index];
        if s.original.is_none() {
            s.original = Some(s.def.clone());
        }
        if let Some(clip) = replacement {
            if let Some(id) = s.voice.take() {
                engine.stop(id);
            }
            s.clip = clip;
        }
        s.def = definition;
        s.pitch_multiplier = pitch_multiplier;
        Ok(())
    }

    pub fn play_entry(
        &mut self,
        engine: &AudioEngine,
        index: usize,
        looping: bool,
        var: &dyn Fn(&str) -> Option<f32>,
        transform: &Mat4,
    ) -> Result<(), String> {
        let s = self
            .sounds
            .get_mut(index)
            .ok_or("sound entry is unavailable")?;
        // Silent/offline engines still maintain real mixer voices, reported with
        // output_enabled=false by the application API.
        let clip = s
            .clip
            .clone()
            .or_else(|| engine.load_clip(&omsi_cfg::resolve_path(&self.dir, &s.def.file)))
            .ok_or("sound has no decodable clip; dynamic entries require a file first")?;
        if let Some(id) = s.voice.take() {
            engine.stop(id);
        }
        s.clip = Some(clip.clone());
        s.control = if looping {
            PlaybackControl::Loop
        } else {
            PlaybackControl::Once
        };
        s.active_since = Some(std::time::Instant::now());
        let parameters = params(
            engine,
            s,
            var,
            transform,
            self.master,
            self.exterior,
            self.muffled,
            !self.listener_vehicle,
        );
        s.voice = Some(engine.play(clip, parameters));
        Ok(())
    }

    pub fn stop_entry(&mut self, engine: &AudioEngine, index: usize) -> Result<(), String> {
        let s = self
            .sounds
            .get_mut(index)
            .ok_or("sound entry is unavailable")?;
        if let Some(id) = s.voice.take() {
            engine.stop(id);
        }
        s.control = PlaybackControl::Stopped;
        Ok(())
    }

    pub fn reset_entry(&mut self, engine: &AudioEngine, index: usize) -> Result<(), String> {
        let s = self.sounds.get(index).ok_or("sound entry is unavailable")?;
        let original = s.original.as_ref().unwrap_or(&s.def).clone();
        self.replace_entry(engine, index, original, 1.0)?;
        let s = &mut self.sounds[index];
        if let Some(id) = s.voice.take() {
            engine.stop(id);
        }
        s.original = None;
        s.control = PlaybackControl::Native;
        s.held = false;
        s.active_since = None;
        s.peak = 0.0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_voice_control_uses_mixer_and_survives_native_updates() {
        let mut engine = AudioEngine::new_silent();
        let cfg = SoundCfg {
            sounds: vec![SoundEntry {
                file: "fixture.wav".into(),
                volume: 0.5,
                important: true,
                triggers: vec!["native_ping".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut sounds = SoundSet::new(&engine, &cfg, Path::new("."));
        sounds.sounds[0].clip = Some(Arc::new(Clip {
            sample_rate: 48000,
            channels: 1,
            samples: vec![100; 480],
        }));
        engine.enabled = true;
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &["native_ping".into()]);
        assert_eq!(sounds.sounds[0].peak, 0.5);
        sounds
            .play_entry(&engine, 0, true, &|_| None, &Mat4::IDENTITY)
            .unwrap();
        assert!(sounds.entry(0, &engine).unwrap().voice.unwrap().0.looping);
        let mut def = cfg.sounds[0].clone();
        def.volume = 0.25;
        sounds.replace_entry(&engine, 0, def, 2.0).unwrap();
        // Exercise the real update path without an output device or OS stream.
        engine.enabled = true;
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &[]);
        engine.enabled = false;
        update_controlled(
            &engine,
            &mut sounds.sounds[0],
            &|_| None,
            &Mat4::IDENTITY,
            1.0,
            false,
            false,
            true,
        );
        let voice = sounds.entry(0, &engine).unwrap().voice.unwrap().0;
        assert_eq!((voice.gain, voice.pitch), (0.25, 2.0));
        assert!(
            voice.important,
            "manual playback preserves authored mixer priority"
        );
        sounds.stop_entry(&engine, 0).unwrap();
        assert!(sounds.entry(0, &engine).unwrap().voice.is_none());
        sounds.reset_entry(&engine, 0).unwrap();
        assert_eq!(
            sounds.sounds[0].peak, 0.0,
            "reset forgets the previous trigger's peak"
        );
        let entry = sounds.entry(0, &engine).unwrap();
        assert_eq!(entry.control, PlaybackControl::Native);
        assert_eq!(entry.definition.volume, 0.5);
    }
    #[test]
    fn failed_clip_change_keeps_definition_and_voice() {
        let engine = AudioEngine::new_silent();
        let cfg = SoundCfg {
            sounds: vec![SoundEntry {
                file: "0".into(),
                volume: 0.3,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut sounds = SoundSet::new(&engine, &cfg, Path::new("."));
        let mut replacement = cfg.sounds[0].clone();
        replacement.file = "__missing_native_api_clip__.wav".into();
        assert!(sounds.replace_entry(&engine, 0, replacement, 1.0).is_err());
        assert_eq!(sounds.entry(0, &engine).unwrap().definition, &cfg.sounds[0]);
        assert_ne!(
            sounds.native_id(),
            SoundSet::new(&engine, &cfg, Path::new(".")).native_id()
        );
    }
    #[test]
    fn triggered_fade_in_starts_a_real_silent_voice_and_its_time_curve() {
        let mut engine = AudioEngine::new_silent();
        let cfg = SoundCfg {
            sounds: vec![SoundEntry {
                file: "fixture.wav".into(),
                volume: 1.0,
                triggers: vec!["bell".into()],
                vol_curves: vec![omsi_vehicle::VolCurve {
                    variable: "-1".into(),
                    points: vec![(0.0, 0.0), (1.0, 1.0)],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut sounds = SoundSet::new(&engine, &cfg, Path::new("."));
        sounds.sounds[0].clip = Some(Arc::new(Clip {
            sample_rate: 48000,
            channels: 1,
            samples: vec![100; 480],
        }));
        engine.enabled = true; // Run the production state update; no stream is opened.
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &["bell".into()]);
        let info = sounds.entry(0, &engine).unwrap();
        assert!(info.active_seconds.is_some());
        assert!(info.voice.is_some());
        assert!(info.voice.unwrap().0.gain < 0.01);
    }
}
