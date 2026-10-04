//! Main-thread scene operations for external simulator clients.
use super::*;

fn valid_values(values: &[(String, f32)]) -> Result<(), String> {
    if values.is_empty() || values.len() > 64 {
        return Err("a scenery update must contain 1..64 variables".into());
    }
    let mut seen = hashbrown::HashSet::new();
    for (name, value) in values {
        if name.is_empty()
            || name.len() > 128
            || !value.is_finite()
            || !seen.insert(name.to_ascii_lowercase())
        {
            return Err("invalid or duplicate scenery variable".into());
        }
    }
    Ok(())
}

/// Source records retain their order when streaming omits objects or Chrono deletes
/// one. This is explicitly a source ordinal, NOT OmsiHook's expanded runtime array.
fn source_object(file: &omsi_cfg::CfgFile, ordinal: usize) -> Result<(i64, String), String> {
    let mut reader = file.reader();
    let mut next = 0;
    while let Some(keyword) = reader.next_keyword() {
        if !matches!(
            keyword.as_str(),
            "object" | "attachobj" | "splineattachement" | "splineattachement_repeater"
        ) {
            continue;
        }
        let current = next;
        next += 1;
        if current != ordinal {
            continue;
        }
        if !matches!(keyword.as_str(), "object" | "attachobj") {
            return Err("a spline attachment row has no unique source object instance".into());
        }
        reader.line();
        let path = reader.str().to_string();
        let id = reader
            .line()
            .trim()
            .parse::<i64>()
            .map_err(|_| "invalid source object id".to_string())?;
        return Ok((id, path));
    }
    Err("source object ordinal is outside this tile".into())
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn light_binding(
    controller: Option<usize>,
    index: usize,
    parent: Option<(i64, usize)>,
    controllers: &HashMap<i64, usize>,
) -> (Option<usize>, usize) {
    match (controller, parent) {
        (Some(controller), _) => (Some(controller), index),
        (None, Some((parent, index))) => (controllers.get(&parent).copied(), index),
        _ => (None, index),
    }
}

impl World {
    /// Resolve the same controller the scenery script tick supplies, including a
    /// crossing on a tile loaded after its child. Keep the authored light index
    /// when that crossing is not loaded yet; no controller is claimed in that case.
    pub(crate) fn bridge_light_binding(&self, object: &ScriptedObject) -> (Option<usize>, usize) {
        light_binding(
            object.controller,
            object.light_index,
            object.light_parent,
            &self.controller_of_object.lock(),
        )
    }

    /// Set source-identified scenery variables atomically. Runtime OMSI object-array
    /// indices require a separate translation; they must never be passed as ordinals.
    pub(crate) fn bridge_scenery_set(
        &self,
        tile_index: usize,
        object_index: usize,
        values: &[(String, f32)],
    ) -> Result<(), String> {
        valid_values(values)?;
        let tile = self
            .global
            .tiles
            .iter()
            .find(|t| t.index == tile_index)
            .ok_or_else(|| "unknown source tile index".to_string())?;
        let path = omsi_cfg::resolve_path(&self.map_dir, &tile.file);
        let file = omsi_cfg::CfgFile::read(path).map_err(|e| e.to_string())?;
        let (id, source_path) = source_object(&file, object_index)?;
        let suffix = format!("/{}", source_path.replace('\\', "/").to_ascii_lowercase());
        let mut objects = self.scripted.lock();
        let mut matches = objects.iter_mut().filter(|o| {
            o.tile == (tile.x, tile.y)
                && o.map_id == id
                && path_key(&o.ty.sco.path).ends_with(&suffix)
        });
        let object = matches
            .next()
            .ok_or_else(|| "source object is not loaded or has no script".to_string())?;
        if matches.next().is_some() {
            return Err("source object resolves to multiple loaded instances".into());
        }
        for (name, _) in values {
            if object.inst.program.var(name).is_none() {
                return Err(format!("source object does not declare variable {name}"));
            }
        }
        for (name, value) in values {
            object.inst.set_var(name, *value);
        }
        Ok(())
    }

    /// Reload a resident vehicle picture, or attach a file explicitly declared by an
    /// active `[texchanges]` master. Parent paths are authorized by that declaration
    /// alone; their canonical target must still be inside this OMSI installation.
    pub(crate) fn bridge_refresh_texture(
        &self,
        renderer: &Renderer,
        scene: &mut Scene,
        player: &mut crate::Player,
        relative_path: &str,
    ) -> Result<usize, String> {
        let mut sections: Vec<(&omsi_sim::VehicleType, &mut VehicleRender)> =
            vec![(&player.vehicle.ty, &mut player.render)];
        sections.extend(player.vehicle.trailers.iter().zip(&mut player.trailer_renders)
            .map(|(part, render)| (part.ty.as_ref(), render)));
        let updated = self.bridge_refresh_texture_sections(renderer, scene, &mut sections, relative_path)?;
        // Select the rebuilt entry through the ordinary variable/material path. A
        // successful refresh must change the live instance, not only a cache record.
        for (_, render) in sections {
            sync_materials(renderer, scene, &player.vehicle, render);
        }
        Ok(updated)
    }

    fn bridge_refresh_texture_sections(
        &self,
        renderer: &Renderer,
        scene: &mut Scene,
        sections: &mut [(&omsi_sim::VehicleType, &mut VehicleRender)],
        relative_path: &str,
    ) -> Result<usize, String> {
        let requested = relative_path.trim().replace('\\', "/");
        let relative = PathBuf::from(&requested);
        if requested.is_empty() || requested.len() > 1024 || relative.is_absolute()
            || requested.contains(':')
            || relative.components().any(|c| matches!(c,
                std::path::Component::RootDir | std::path::Component::Prefix(_)))
        {
            return Err("texture path must be an authored relative path".into());
        }
        let root = self.root.canonicalize().map_err(|e| e.to_string())?;
        let mut bindings = Vec::new();
        let mut declared_path: Option<PathBuf> = None;
        for (section, (ty, render)) in sections.iter().enumerate() {
            for (variant, slot) in render.variants.iter().enumerate() {
                let Some(master) = ty.texchange(&slot.tex_key) else { continue };
                for (entry, authored) in master.entries.iter().enumerate() {
                    if path_key(Path::new(authored.trim())) != path_key(&relative)
                        || entry >= slot.entries.len() || entry >= slot.entry_tex.len()
                    { continue; }
                    if render.instances.get(slot.mesh).and_then(|id| scene.instances.get(*id))
                        .and_then(|instance| instance.materials.get(slot.slot)).is_none()
                    {
                        return Err("declared texture has no live material slot".into());
                    }
                    let dirs = std::iter::once(master.dir.clone())
                        .chain(ty.texture_dirs(&self.root).into_iter().take(3));
                    let path = dirs.filter_map(|d| omsi_cfg::resolve_path(&d, &requested).canonicalize().ok())
                        .find(|p| p.is_file() && p.starts_with(&root))
                        .ok_or("declared texture is not a file inside the OMSI root")?;
                    if declared_path.as_ref().is_some_and(|previous| path_key(previous) != path_key(&path)) {
                        return Err("texture declaration is ambiguous between vehicle sections".into());
                    }
                    declared_path = Some(path);
                    bindings.push((section, variant, entry));
                }
            }
        }
        let declared = declared_path.is_some();
        let path = if let Some(path) = declared_path { path } else {
            if relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err("parent texture path is not declared by the active vehicle".into());
            }
            let dirs: Vec<PathBuf> = sections.iter()
                .flat_map(|(ty, _)| ty.texture_dirs(&self.root).into_iter().take(3)).collect();
            let canonical_dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
            dirs.iter().find_map(|d| {
                let p = omsi_cfg::resolve_path(d, &requested).canonicalize().ok()?;
                (p.is_file() && canonical_dirs.iter().any(|d| p.starts_with(d))).then_some(p)
            }).ok_or("texture is not a file inside the active vehicle texture directories")?
        };
        let canonical_key = path_key(&path);
        let same_file = |p: &Path| p.canonicalize().ok().is_some_and(|p| path_key(&p) == canonical_key);
        let mut resident: Vec<(PathBuf, TextureId)> = self.vehicle_textures.lock().iter()
            .filter(|(p, _)| same_file(p)).map(|(p, (id, _))| (p.clone(), *id)).collect();
        resident.extend(self.gpu.lock().textures.iter().filter(|(p, _)| same_file(p))
            .map(|(p, entry)| (p.clone(), entry.id)));
        if resident.is_empty() && !declared {
            return Err("texture is not currently resident in the scene".into());
        }
        let image = omsi_texture::decode_file(&path).map_err(|e| e.to_string())?;
        if image.width == 0 || image.height == 0 || image.width > 4096 || image.height > 4096 {
            return Err("refreshed texture dimensions exceed 4096 by 4096".into());
        }
        {
            let mut pinned = self.bridge_texture_paths.lock();
            let mut required: hashbrown::HashSet<PathBuf> = resident.iter().map(|(p, _)| p.clone()).collect();
            if declared { required.insert(path.clone()); }
            if pinned.len() + required.iter().filter(|p| !pinned.contains(*p)).count() > 64 {
                return Err("external file texture limit reached".into());
            }
            pinned.extend(required);
        }
        let data = TextureData::from_image(image);
        let mut ids: Vec<TextureId> = resident.iter().map(|(_, id)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        for id in &ids { renderer.replace_texture(scene, *id, &data); }
        for (alias, id) in &resident {
            self.textures.release(alias);
            if let Some(entry) = self.gpu.lock().textures.get_mut(alias) {
                entry.bytes = scene.texture_bytes_of(*id);
                entry.format = data.format;
                entry.dropped = 0;
            }
        }
        if declared {
            let mut shared = self.vehicle_textures.lock();
            // A scenery texture can own the same file. Give newly attached vehicle
            // slots a counted vehicle resource, so release_vehicle cannot free scenery.
            let held_path = shared.keys().find(|p| same_file(p)).cloned().unwrap_or_else(|| path.clone());
            let id = if let Some((id, _)) = shared.get(&held_path) { *id } else {
                let id = renderer.add_texture_data(scene, &data);
                attach_pbr(renderer, scene, &held_path, id);
                shared.insert(held_path.clone(), (id, 0));
                ids.push(id);
                id
            };
            for (section, variant, entry) in bindings {
                let render = &mut sections[section].1;
                let slot = &mut render.variants[variant];
                if slot.entry_tex[entry] == Some(id) { continue; }
                let pair = slot.spec.build(renderer, scene, Some(id));
                render.own_materials.extend([pair.0, pair.1]);
                slot.entries[entry] = pair;
                slot.entry_tex[entry] = Some(id);
                if entry == 0 {
                    slot.base_tex = Some(id);
                    slot.base = pair.0;
                    slot.item = pair.1;
                }
                if !slot.bridge_textures.contains(&held_path) {
                    shared.get_mut(&held_path).unwrap().1 += 1;
                    slot.bridge_textures.push(held_path.clone());
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        renderer.rebind_textures(scene, &ids);
        Ok(ids.len())
    }

}

fn validate_frame(index: usize, width: u32, height: u32, bytes: usize) -> Result<(), String> {
    if index >= 64
        || width == 0
        || height == 0
        || width > 2048
        || height > 2048
        || bytes != width as usize * height as usize * 4
    {
        return Err("invalid script texture index, dimensions or RGBA byte count".into());
    }
    Ok(())
}

/// Replace a declared script texture in place, preserving all front/trailer material
/// references and the ordinary vehicle resource cleanup. Inputs are top-down RGBA.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bridge_script_texture(
    renderer: &Renderer,
    scene: &mut Scene,
    player: &mut crate::Player,
    section: usize,
    index: usize,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<(), String> {
    validate_frame(index, width, height, rgba.len())?;
    let render = if section == 0 {
        &player.render
    } else {
        player
            .trailer_renders
            .get(section - 1)
            .ok_or_else(|| "vehicle section is not present".to_string())?
    };
    let id = render
        .script_textures
        .get(index)
        .copied()
        .flatten()
        .ok_or_else(|| "vehicle section does not declare this script texture".to_string())?;
    if renderer.texture_levels(scene, id).is_none() {
        return Err("script texture GPU resource is no longer live".into());
    }
    let image = Image {
        width,
        height,
        rgba,
        has_alpha: true,
    };
    if renderer.texture_levels(scene, id) != Some((width, height, 1)) {
        renderer.replace_texture(
            scene,
            id,
            &TextureData {
                gpu_mips: false,
                ..TextureData::from_image(image)
            },
        );
        renderer.rebind_textures(scene, &[id]);
    } else {
        renderer.update_texture(scene, id, &image);
    }
    // A rear section may share the leading section's script texture IDs. Mark every
    // alias, so a bus-script redraw through another section cannot overwrite the frame.
    for render in std::iter::once(&mut player.render).chain(player.trailer_renders.iter_mut()) {
        for (slot, texture) in render.script_textures.iter().enumerate() {
            if *texture == Some(id) {
                render.external_script_textures.insert(slot);
            }
        }
    }
    Ok(())
}

pub(crate) fn bridge_release_script_texture(
    renderer: &Renderer,
    scene: &mut Scene,
    player: &mut crate::Player,
    section: usize,
    index: usize,
) -> Result<(), String> {
    let render = if section == 0 {
        &player.render
    } else {
        player
            .trailer_renders
            .get(section - 1)
            .ok_or_else(|| "vehicle section is not present".to_string())?
    };
    let id = render
        .script_textures
        .get(index)
        .copied()
        .flatten()
        .ok_or_else(|| "vehicle section does not declare this script texture".to_string())?;
    if renderer.texture_levels(scene, id).is_none() {
        return Err("script texture GPU resource is no longer live".into());
    }
    let mut original = None;
    for (slot, texture) in player.render.script_textures.iter().enumerate() {
        if *texture == Some(id) {
            if let Some(st) = player.vehicle.host.script_textures.get(slot) {
                original = Some((
                    Image {
                        width: st.width,
                        height: st.height,
                        rgba: st.rgba.clone(),
                        has_alpha: true,
                    },
                    st.mipmaps,
                ));
            }
        }
    }
    if original.is_none() {
        for (part, render) in player.vehicle.trailers.iter().zip(&player.trailer_renders) {
            if render.shared_script {
                continue;
            }
            for (slot, texture) in render.script_textures.iter().enumerate() {
                if *texture != Some(id) {
                    continue;
                }
                if let Some(&(width, height)) = part.ty.model.script_textures.get(slot) {
                    let (width, height) = (width.max(1) as u32, height.max(1) as u32);
                    original = Some((
                        Image {
                            width,
                            height,
                            rgba: vec![0; width as usize * height as usize * 4],
                            has_alpha: true,
                        },
                        false,
                    ));
                }
            }
        }
    }
    let (image, mipmaps) =
        original.ok_or_else(|| "original script texture is unavailable".to_string())?;
    if mipmaps {
        if renderer.update_texture_mips(scene, id, &image) {
            renderer.rebind_textures(scene, &[id]);
        }
    } else {
        renderer.replace_texture(
            scene,
            id,
            &TextureData {
                gpu_mips: false,
                ..TextureData::from_image(image)
            },
        );
        renderer.rebind_textures(scene, &[id]);
    }
    for render in std::iter::once(&mut player.render).chain(player.trailer_renders.iter_mut()) {
        for (slot, texture) in render.script_textures.iter().enumerate() {
            if *texture == Some(id) {
                render.external_script_textures.remove(&slot);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_api_light_binding_resolves_late_parents_without_overriding_own_controller() {
        let crossings = [42].into_iter().collect();
        assert_eq!(light_child_of(&crossings, Some(42), &["3".into()]), Some((42, 3)));
        assert_eq!(light_child_of(&crossings, Some(42), &["-1".into()]), None);
        assert_eq!(light_child_of(&crossings, Some(99), &["3".into()]), None);
        assert_eq!(light_child_of(&crossings, None, &["3".into()]), None);
        assert_eq!(light_child_of(&crossings, Some(42), &["stop name".into()]), None);
        let mut controllers = HashMap::new();
        assert_eq!(
            light_binding(None, 0, Some((42, 3)), &controllers),
            (None, 3)
        );
        controllers.insert(42, 7);
        assert_eq!(
            light_binding(None, 0, Some((42, 3)), &controllers),
            (Some(7), 3)
        );
        assert_eq!(
            light_binding(Some(9), 2, Some((42, 3)), &controllers),
            (Some(9), 2)
        );
        assert_eq!(light_binding(None, 0, None, &controllers), (None, 0));
        controllers.remove(&42);
        assert_eq!(
            light_binding(None, 0, Some((42, 3)), &controllers),
            (None, 3)
        );
    }

    #[test]
    fn source_ordinals_count_attachment_rows_without_guessing_an_instance() {
        let file = omsi_cfg::CfgFile::from_str("tile.map", "[object]\n0\na.sco\n10\n[splineAttachement]\n0\nrow.sco\n11\n[attachObj]\n0\nb.sco\n12\n");
        assert_eq!(source_object(&file, 0).unwrap(), (10, "a.sco".into()));
        assert!(source_object(&file, 1).is_err());
        assert_eq!(source_object(&file, 2).unwrap(), (12, "b.sco".into()));
        assert!(source_object(&file, 3).is_err());
    }

    #[test]
    fn frames_reject_invalid_sizes_indices_and_partial_pixels() {
        assert!(validate_frame(2, 640, 480, 640 * 480 * 4).is_ok());
        assert!(validate_frame(4, 2048, 2048, 2048 * 2048 * 4).is_ok());
        for (i, w, h, n) in [
            (64, 1, 1, 4),
            (2, 0, 1, 0),
            (2, 4096, 1, 16384),
            (2, 1, 1, 3),
        ] {
            assert!(validate_frame(i, w, h, n).is_err());
        }
    }

    #[test]
    fn scenery_batches_reject_partial_or_ambiguous_updates() {
        assert!(valid_values(&[("rychlost".into(), 35.0)]).is_ok());
        assert!(valid_values(&[("rychlost".into(), f32::NAN)]).is_err());
        assert!(valid_values(&[("rychlost".into(), 1.0), ("RYCHLOST".into(), 2.0)]).is_err());
        assert!(valid_values(&[]).is_err());
    }
}

#[cfg(test)]
#[path = "bridge_texture_refresh.rs"]
mod texture_refresh;
