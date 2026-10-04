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
    s: &mut RuntimeSound,
    var: &dyn Fn(&str) -> Option<f32>,
    transform: &Mat4,
    ctx: &Ctx,
) -> VoiceParams {
    let evaluated = ctx.eval_controlled(s, var, transform);
    let mut parameters = ctx.params(s, &evaluated, s.control == PlaybackControl::Loop, transform);
    // Explicit starts, including a silent fade-in, own a mixer voice. They still
    // silence a frequency or volume below DirectSound's playback threshold.
    if !evaluated.audible {
        parameters.gain = 0.0;
    }
    parameters.pitch = parameters.pitch.clamp(0.001, 64.0);
    parameters
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
    pub fn native_id(&self) -> u64 {
        self.api_id
    }
    pub fn directory(&self) -> &Path {
        &self.dir
    }
    pub fn is_external(&self, index: usize) -> bool {
        self.sounds.get(index).is_some_and(|s| s.external)
    }
    pub fn clip_index(&self, lease: &str) -> Option<usize> {
        self.sounds.iter().position(|s| s.external && s.lease.as_deref() == Some(lease))
    }

    /// Append a bounded, leased announcement voice. Released slots are reused;
    /// the authored entries retain their indices, triggers and definitions.
    pub fn play_owned_clip(
        &mut self,
        engine: &AudioEngine,
        definition: SoundEntry,
        clip: Arc<Clip>,
        lease: String,
        var: &dyn Fn(&str) -> Option<f32>,
        transform: &Mat4,
    ) -> Result<usize, String> {
        if clip.sample_rate == 0 || clip.channels == 0 || clip.frames() == 0 {
            return Err("announcement clip has no audio frames".into());
        }
        if lease.is_empty() || self.clip_index(&lease).is_some() {
            return Err("announcement lease is invalid".into());
        }
        if self.sounds.iter().filter(|s| s.external && s.lease.is_some()).count() >= 8 {
            return Err("announcement voice limit reached".into());
        }
        let mut sound = RuntimeSound::new(definition, Some(clip));
        sound.external = true;
        sound.lease = Some(lease);
        sound.control = PlaybackControl::Stopped;
        let index = if let Some(index) = self.sounds.iter().position(|s| s.external && s.lease.is_none()) {
            self.sounds[index] = sound;
            index
        } else {
            self.sounds.push(sound);
            self.sounds.len() - 1
        };
        self.play_entry(engine, index, false, var, transform)?;
        Ok(index)
    }

    pub fn release_owned_clip(&mut self, engine: &AudioEngine, lease: &str) -> Result<(), String> {
        let index = self.clip_index(lease).ok_or("announcement lease is no longer active")?;
        self.stop_entry(engine, index)?;
        let sound = &mut self.sounds[index];
        sound.clip = None;
        sound.lease = None;
        sound.def = SoundEntry::default();
        sound.active_since = None;
        Ok(())
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
            s.last_gain = 1.0;
            s.last_pitch = 1.0;
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
        s.active_since = None;
        s.last_gain = 1.0;
        s.last_pitch = 1.0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn announcement() -> Arc<Clip> {
        Arc::new(Clip { sample_rate: 48000, channels: 1, samples: vec![1024; 480] })
    }
    #[test]
    fn owned_announcements_preserve_authored_sounds_and_finish_in_the_mixer() {
        let mut engine = AudioEngine::new_silent();
        engine.enabled = true; // Production mixer, without opening a speaker device.
        let authored = SoundEntry { file: "0".into(), volume: 0.6, triggers: vec!["brake".into()], ..Default::default() };
        let mut sounds = SoundSet::new(&engine, &SoundCfg { sounds: vec![authored.clone()], ..Default::default() }, Path::new("."));
        sounds.master = 0.5;
        let index = sounds.play_owned_clip(&engine, SoundEntry { volume: 0.4, important: true, ..Default::default() }, announcement(), "first".into(), &|_| None, &Mat4::IDENTITY).unwrap();
        assert_eq!(index, 1);
        assert_eq!(sounds.entry(0, &engine).unwrap().definition, &authored);
        let params = sounds.entry(index, &engine).unwrap().voice.unwrap().0;
        assert!((params.gain - 0.2).abs() < 0.001);
        assert!(!params.looping && params.important && params.position.is_none());
        let mut output = vec![0.0; 2048];
        engine.render_offline(&mut output);
        assert!(output.iter().any(|sample| *sample != 0.0));
        assert!(sounds.entry(index, &engine).unwrap().voice.is_none());
        sounds.release_owned_clip(&engine, "first").unwrap();
        let next = sounds.play_owned_clip(&engine, SoundEntry::default(), announcement(), "second".into(), &|_| None, &Mat4::IDENTITY).unwrap();
        assert_eq!(next, index);
        assert!(sounds.release_owned_clip(&engine, "first").is_err());
        assert!(sounds.entry(next, &engine).unwrap().voice.is_some());
        sounds.release_owned_clip(&engine, "second").unwrap();
        assert!(sounds.entry(next, &engine).unwrap().voice.is_none());
        assert!(sounds.has_trigger("brake"));
    }
    #[test]
    fn owned_announcement_slots_are_bounded_and_bad_clips_do_not_mutate_them() {
        let engine = AudioEngine::new_silent();
        let mut sounds = SoundSet::new(&engine, &SoundCfg::default(), Path::new("."));
        let empty = Arc::new(Clip { sample_rate: 48000, channels: 1, samples: Vec::new() });
        assert!(sounds.play_owned_clip(&engine, SoundEntry::default(), empty, "empty".into(), &|_| None, &Mat4::IDENTITY).is_err());
        assert_eq!(sounds.len(), 0);
        for i in 0..8 {
            sounds.play_owned_clip(&engine, SoundEntry::default(), announcement(), format!("lease-{i}"), &|_| None, &Mat4::IDENTITY).unwrap();
        }
        assert!(sounds.play_owned_clip(&engine, SoundEntry::default(), announcement(), "overflow".into(), &|_| None, &Mat4::IDENTITY).is_err());
        assert_eq!(sounds.len(), 8);
        sounds.release_owned_clip(&engine, "lease-3").unwrap();
        assert_eq!(sounds.play_owned_clip(&engine, SoundEntry::default(), announcement(), "replacement".into(), &|_| None, &Mat4::IDENTITY).unwrap(), 3);
        assert_eq!(sounds.len(), 8);
    }
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
        // Native triggers keep the upstream GetTickCount-based -1 input, already
        // beyond the curve's last point; explicit playback keeps a separate clock.
        let info = sounds.entry(0, &engine).unwrap();
        assert!(info.active_seconds.is_none());
        assert_eq!(info.voice.unwrap().0.gain, 1.0);
        // Explicit API playback has its own start time, including a silent fade-in.
        sounds
            .play_entry(&engine, 0, false, &|_| None, &Mat4::IDENTITY)
            .unwrap();
        let info = sounds.entry(0, &engine).unwrap();
        assert!(info.active_seconds.is_some());
        assert!(info.voice.unwrap().0.gain < 0.05);
    }
}
