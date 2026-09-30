//! Main-thread scene operations for external simulator clients.
use super::*;

fn valid_values(values: &[(String, f32)]) -> Result<(), String> {
    if values.is_empty() || values.len() > 64 {
        return Err("a scenery update must contain 1..64 variables".into());
    }
    let mut seen = hashbrown::HashSet::new();
    for (name, value) in values {
        if name.is_empty() || name.len() > 128 || !value.is_finite()
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
        if !matches!(keyword.as_str(), "object" | "attachobj" | "splineattachement" | "splineattachement_repeater") {
            continue;
        }
        let current = next;
        next += 1;
        if current != ordinal { continue; }
        if !matches!(keyword.as_str(), "object" | "attachobj") {
            return Err("a spline attachment row has no unique source object instance".into());
        }
        reader.line();
        let path = reader.str().to_string();
        let id = reader.line().trim().parse::<i64>()
            .map_err(|_| "invalid source object id".to_string())?;
        return Ok((id, path));
    }
    Err("source object ordinal is outside this tile".into())
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_ascii_lowercase()
}

impl World {
    /// Set source-identified scenery variables atomically. Runtime OMSI object-array
    /// indices require a separate translation; they must never be passed as ordinals.
    pub(crate) fn bridge_scenery_set(
        &self, tile_index: usize, object_index: usize, values: &[(String, f32)],
    ) -> Result<(), String> {
        valid_values(values)?;
        let tile = self.global.tiles.iter().find(|t| t.index == tile_index)
            .ok_or_else(|| "unknown source tile index".to_string())?;
        let path = omsi_cfg::resolve_path(&self.map_dir, &tile.file);
        let file = omsi_cfg::CfgFile::read(path).map_err(|e| e.to_string())?;
        let (id, source_path) = source_object(&file, object_index)?;
        let suffix = format!("/{}", source_path.replace('\\', "/").to_ascii_lowercase());
        let mut objects = self.scripted.lock();
        let mut matches = objects.iter_mut().filter(|o| o.tile == (tile.x, tile.y)
            && o.map_id == id && path_key(&o.ty.sco.path).ends_with(&suffix));
        let object = matches.next().ok_or_else(|| "source object is not loaded or has no script".to_string())?;
        if matches.next().is_some() {
            return Err("source object resolves to multiple loaded instances".into());
        }
        for (name, _) in values {
            if object.inst.program.var(name).is_none() {
                return Err(format!("source object does not declare variable {name}"));
            }
        }
        for (name, value) in values { object.inst.set_var(name, *value); }
        Ok(())
    }

    /// Reload an already resident file after a card/display producer replaces its bytes.
    /// The path is resolved inside the active vehicle sections' texture directories.
    pub(crate) fn bridge_refresh_texture(
        &self, renderer: &Renderer, scene: &mut Scene, player: &crate::Player,
        relative_path: &str,
    ) -> Result<usize, String> {
        let relative = PathBuf::from(relative_path.replace('\\', "/"));
        if relative_path.len() > 1024 || relative.as_os_str().is_empty()
            || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("texture path must be relative without parent components".into());
        }
        let dirs: Vec<PathBuf> = std::iter::once(&player.vehicle.ty)
            .chain(player.vehicle.trailers.iter().map(|t| &t.ty))
            .flat_map(|ty| ty.texture_dirs(&self.root).into_iter().take(3))
            .collect();
        let canonical_dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
        let path = dirs.iter().find_map(|d| {
            let p = omsi_cfg::resolve_path(d, &relative.to_string_lossy());
            let canonical = p.canonicalize().ok()?;
            (canonical.is_file() && canonical_dirs.iter().any(|d| canonical.starts_with(d)))
                .then_some(canonical)
        }).ok_or_else(|| "texture is not a file inside the active vehicle texture directories".to_string())?;
        let canonical_key = path_key(&path);
        let same_file = |p: &Path| p.canonicalize().ok().is_some_and(|p| path_key(&p) == canonical_key);
        let mut resident: Vec<(PathBuf, TextureId)> = self.vehicle_textures.lock().iter()
            .filter(|(p, _)| same_file(p)).map(|(p, (id, _))| (p.clone(), *id)).collect();
        resident.extend(self.gpu.lock().textures.iter().filter(|(p, _)| same_file(p))
            .map(|(p, entry)| (p.clone(), entry.id)));
        if resident.is_empty() { return Err("texture is not currently resident in the scene".into()); }
        let image = omsi_texture::decode_file(&path).map_err(|e| e.to_string())?;
        if image.width == 0 || image.height == 0 || image.width > 4096 || image.height > 4096 {
            return Err("refreshed texture dimensions exceed 4096 by 4096".into());
        }
        {
            let mut pinned = self.bridge_texture_paths.lock();
            let new = resident.iter().filter(|(p, _)| !pinned.contains(p)).count();
            if pinned.len() + new > 64 { return Err("external file texture limit reached".into()); }
            pinned.extend(resident.iter().map(|(p, _)| p.clone()));
        }
        let data = TextureData::from_image(image);
        let mut ids: Vec<TextureId> = resident.iter().map(|(_, id)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        for id in &ids { renderer.replace_texture(scene, *id, &data); }
        for (path, id) in resident {
            self.textures.release(&path);
            if let Some(entry) = self.gpu.lock().textures.get_mut(&path) {
                entry.bytes = scene.texture_bytes_of(id);
                entry.format = data.format;
                entry.dropped = 0;
            }
        }
        renderer.rebind_textures(scene, &ids);
        Ok(ids.len())
    }
}

fn validate_frame(index: usize, width: u32, height: u32, bytes: usize) -> Result<(), String> {
    if index >= 64 || width == 0 || height == 0 || width > 2048 || height > 2048
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
    renderer: &Renderer, scene: &mut Scene, player: &mut crate::Player,
    section: usize, index: usize, width: u32, height: u32, rgba: Vec<u8>,
) -> Result<(), String> {
    validate_frame(index, width, height, rgba.len())?;
    let render = if section == 0 { &player.render } else {
        player.trailer_renders.get(section - 1)
            .ok_or_else(|| "vehicle section is not present".to_string())?
    };
    let id = render.script_textures.get(index).copied().flatten()
        .ok_or_else(|| "vehicle section does not declare this script texture".to_string())?;
    if renderer.texture_levels(scene, id).is_none() {
        return Err("script texture GPU resource is no longer live".into());
    }
    let image = Image { width, height, rgba, has_alpha: true };
    if renderer.texture_levels(scene, id) != Some((width, height, 1)) {
        renderer.replace_texture(scene, id, &TextureData {
            gpu_mips: false, ..TextureData::from_image(image)
        });
        renderer.rebind_textures(scene, &[id]);
    } else {
        renderer.update_texture(scene, id, &image);
    }
    // A rear section may share the leading section's script texture IDs. Mark every
    // alias, so a bus-script redraw through another section cannot overwrite the frame.
    for render in std::iter::once(&mut player.render).chain(player.trailer_renders.iter_mut()) {
        for (slot, texture) in render.script_textures.iter().enumerate() {
            if *texture == Some(id) { render.external_script_textures.insert(slot); }
        }
    }
    Ok(())
}

pub(crate) fn bridge_release_script_texture(
    renderer: &Renderer, scene: &mut Scene, player: &mut crate::Player,
    section: usize, index: usize,
) -> Result<(), String> {
    let render = if section == 0 { &player.render } else {
        player.trailer_renders.get(section - 1)
            .ok_or_else(|| "vehicle section is not present".to_string())?
    };
    let id = render.script_textures.get(index).copied().flatten()
        .ok_or_else(|| "vehicle section does not declare this script texture".to_string())?;
    if renderer.texture_levels(scene, id).is_none() {
        return Err("script texture GPU resource is no longer live".into());
    }
    let mut original = None;
    for (slot, texture) in player.render.script_textures.iter().enumerate() {
        if *texture == Some(id) {
            if let Some(st) = player.vehicle.host.script_textures.get(slot) {
                original = Some((Image { width: st.width, height: st.height,
                    rgba: st.rgba.clone(), has_alpha: true }, st.mipmaps));
            }
        }
    }
    if original.is_none() {
        for (part, render) in player.vehicle.trailers.iter().zip(&player.trailer_renders) {
            if render.shared_script { continue; }
            for (slot, texture) in render.script_textures.iter().enumerate() {
                if *texture != Some(id) { continue; }
                if let Some(&(width, height)) = part.ty.model.script_textures.get(slot) {
                    let (width, height) = (width.max(1) as u32, height.max(1) as u32);
                    original = Some((Image { width, height,
                        rgba: vec![0; width as usize * height as usize * 4], has_alpha: true }, false));
                }
            }
        }
    }
    let (image, mipmaps) = original.ok_or_else(|| "original script texture is unavailable".to_string())?;
    if mipmaps {
        if renderer.update_texture_mips(scene, id, &image) { renderer.rebind_textures(scene, &[id]); }
    } else {
        renderer.replace_texture(scene, id, &TextureData {
            gpu_mips: false, ..TextureData::from_image(image)
        });
        renderer.rebind_textures(scene, &[id]);
    }
    for render in std::iter::once(&mut player.render).chain(player.trailer_renders.iter_mut()) {
        for (slot, texture) in render.script_textures.iter().enumerate() {
            if *texture == Some(id) { render.external_script_textures.remove(&slot); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for (i, w, h, n) in [(64, 1, 1, 4), (2, 0, 1, 0), (2, 4096, 1, 16384), (2, 1, 1, 3)] {
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
