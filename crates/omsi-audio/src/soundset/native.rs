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

// Keep the v1098 evaluator and native peak-hold behavior. Explicit API voices use
// its curves, pitch and cabin attenuation without its automatic start/view tests.
pub(super) struct Ctx {
    master: f32,
    muffled: bool,
    exterior: bool,
    doppler: bool,
    listener: Vec3,
}

fn params(
    s: &mut RuntimeSound,
    var: &dyn Fn(&str) -> Option<f32>,
    transform: &Mat4,
    ctx: &Ctx,
) -> VoiceParams {
    let position = s.def.pos.map(|p| transform.transform_point3(Vec3::from_array(p)));
    let facing = match (position, s.def.dir) {
        (Some(at), Some(direction)) => transform.transform_vector3(Vec3::from_array(direction))
            .normalize_or_zero().dot((ctx.listener - at).normalize_or_zero()),
        _ => 1.0,
    };
    let active = s.active_since.map_or(0.0, |t| t.elapsed().as_secs_f32());
    let mut volume = s.def.volume;
    for vc in &s.def.vol_curves {
        if let Some(x) = SoundSet::curve_input(vc, var, active, facing) {
            volume *= curve(&vc.points, x);
        }
    }
    let (pitch, fast_enough) = s.clip.as_ref()
        .map(|clip| SoundSet::pitch_of(&s.def, var, clip)).unwrap_or((1.0, true));
    s.last_gain = volume.clamp(0.0, 1.0);
    s.last_pitch = pitch;
    VoiceParams {
        gain: if fast_enough { s.last_gain * ctx.master * SoundSet::outside_gain(ctx.muffled, ctx.exterior) } else { 0.0 },
        pitch: (pitch * s.pitch_multiplier).clamp(0.001, 64.0),
        looping: s.control == PlaybackControl::Loop,
        position,
        doppler: ctx.doppler && s.def.is_loop,
        range: if s.def.range > 0.0 { s.def.range } else if ctx.exterior { 40.0 } else { 5.0 },
        lowpass_hz: SoundSet::lowpass_of(ctx.muffled, ctx.exterior),
        important: s.def.important,
    }
}

pub(super) fn update_controlled(
    engine: &AudioEngine,
    s: &mut RuntimeSound,
    var: &dyn Fn(&str) -> Option<f32>,
    transform: &Mat4,
    ctx: &Ctx,
) {
    if s.control == PlaybackControl::Stopped {
        if let Some(id) = s.voice.take() {
            engine.stop(id);
        }
        return;
    }
    let p = params(s, var, transform, ctx);
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
    pub(super) fn ctx(&self, engine: &AudioEngine) -> Ctx {
        Ctx { master: self.master, muffled: self.muffled, exterior: self.exterior,
            doppler: !self.listener_vehicle, listener: engine.listener_position() }
    }

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
        let ctx = self.ctx(engine);
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
        let parameters = params(s, var, transform, &ctx);
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
        s.peak = 0.0;
        s.active_since = None;
        s.last_gain = 1.0;
        s.last_pitch = 1.0;
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
        assert!((sounds.sounds[0].last_gain - 0.5).abs() < 0.001);
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
        let ctx = sounds.ctx(&engine);
        update_controlled(
            &engine,
            &mut sounds.sounds[0],
            &|_| None,
            &Mat4::IDENTITY,
            &ctx,
        );
        let voice = sounds.entry(0, &engine).unwrap().voice.unwrap().0;
        assert!((voice.gain - 0.25).abs() < 0.001);
        assert_eq!(voice.pitch, 2.0);
        assert!(
            voice.important,
            "manual playback preserves authored mixer priority"
        );
        // Explicit API pitch keeps its advertised bounds even when the native
        // loop's frequency and a valid API multiplier together exceed them.
        for (speed, multiplier, expected) in [(4.0, 32.0, 64.0), (0.05, 0.01, 0.001)] {
            let mut def = cfg.sounds[0].clone();
            def.is_loop = true;
            def.sample_rate = 48000.0;
            def.pitch_ref = 1.0;
            def.pitch_variable = "speed".into();
            sounds.replace_entry(&engine, 0, def, multiplier).unwrap();
            update_controlled(
                &engine,
                &mut sounds.sounds[0],
                &|_| Some(speed),
                &Mat4::IDENTITY,
                &ctx,
            );
            assert_eq!(
                sounds.entry(0, &engine).unwrap().voice.unwrap().0.pitch,
                expected
            );
        }
        sounds.stop_entry(&engine, 0).unwrap();
        assert!(sounds.entry(0, &engine).unwrap().voice.is_none());
        sounds.reset_entry(&engine, 0).unwrap();
        assert_eq!(
            (sounds.sounds[0].last_gain, sounds.sounds[0].last_pitch),
            (1.0, 1.0),
            "reset restores the native buffer defaults"
        );
        let entry = sounds.entry(0, &engine).unwrap();
        assert_eq!(entry.control, PlaybackControl::Native);
        assert_eq!(entry.definition.volume, 0.5);
    }
    #[test]
    fn explicit_playback_bypasses_view_without_inventing_spatial_effects() {
        let mut engine = AudioEngine::new_silent();
        let cfg = SoundCfg {
            sounds: vec![SoundEntry {
                file: "fixture.wav".into(),
                volume: 0.25,
                viewpoint: 2,
                triggers: vec!["ping".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut sounds = SoundSet::new_exterior(&engine, &cfg, Path::new("."));
        sounds.sounds[0].clip = Some(Arc::new(Clip {
            sample_rate: 48000,
            channels: 1,
            samples: vec![100; 480],
        }));
        engine.enabled = true;
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &["ping".into()]);
        assert!(sounds.entry(0, &engine).unwrap().voice.is_none());
        sounds
            .play_entry(&engine, 0, true, &|_| None, &Mat4::IDENTITY)
            .unwrap();
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &[]);
        let voice = sounds.entry(0, &engine).unwrap().voice.unwrap().0;
        assert!(
            voice.position.is_none(),
            "a sound without [3d] stays nonspatial on an AI vehicle"
        );
        assert_eq!(voice.lowpass_hz, 0.0);
        assert!(!voice.doppler, "a [sound] keeps its playback rate");
        assert!((voice.gain - 0.25).abs() < 0.001);
        sounds.reset_entry(&engine, 0).unwrap();
        sounds.update(&engine, &|_| None, &Mat4::IDENTITY, &["ping".into()]);
        assert!(sounds.entry(0, &engine).unwrap().voice.is_none());
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
    fn explicit_playback_uses_its_time_curve_without_changing_native_triggers() {
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
        // In v1098 a native trigger has no elapsed active interval: its -1 curve
        // starts at zero and this fade-in remains silent. Preserve that behavior.
        let info = sounds.entry(0, &engine).unwrap();
        assert!(info.active_seconds.is_none());
        assert!(info.voice.is_none());
        // Explicit API playback has its own start time, including a silent fade-in.
        sounds
            .play_entry(&engine, 0, false, &|_| None, &Mat4::IDENTITY)
            .unwrap();
        let info = sounds.entry(0, &engine).unwrap();
        assert!(info.active_seconds.is_some());
        assert!(info.voice.unwrap().0.gain < 0.05);
    }
}
