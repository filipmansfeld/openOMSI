//! Refresh an existing vehicle texture after an external producer replaces its file.
use super::*;

fn relative_texture_path(name: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(name.replace('\\', "/"));
    if name.len() > 1024
        || name.trim().is_empty()
        || name.contains(':')
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("texture path must be relative without parent components or a drive".into());
    }
    Ok(path)
}

fn confined_texture_path(dirs: &[PathBuf], relative: &Path) -> Option<PathBuf> {
    let roots: Vec<_> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    dirs.iter().find_map(|dir| {
        let path = omsi_cfg::resolve_path(dir, &relative.to_string_lossy())
            .canonicalize()
            .ok()?;
        (path.is_file() && roots.iter().any(|root| path.starts_with(root))).then_some(path)
    })
}

impl World {
    /// Refresh a resident disk texture in the active vehicle's own directories, including
    /// its trailers. Existing materials keep their texture IDs and declared alpha modes.
    /// Main-thread only: completion means replacement and rebinding have been submitted.
    pub(crate) fn refresh_vehicle_texture(
        &self,
        renderer: &Renderer,
        scene: &mut Scene,
        player: &crate::Player,
        relative_path: &str,
    ) -> Result<usize, String> {
        let relative = relative_texture_path(relative_path)?;
        let dirs: Vec<_> = std::iter::once(&player.vehicle.ty)
            .chain(player.vehicle.trailers.iter().map(|t| &t.ty))
            // Exclude the global fallback directory shared by unrelated content.
            .flat_map(|ty| ty.texture_dirs(&self.root).into_iter().take(3))
            .collect();
        let path = confined_texture_path(&dirs, &relative).ok_or_else(|| {
            "texture is not a file inside the active vehicle texture directories".to_string()
        })?;
        let same_file = |candidate: &Path| {
            candidate.canonicalize().ok().is_some_and(|p| {
                if cfg!(windows) {
                    p.to_string_lossy()
                        .eq_ignore_ascii_case(&path.to_string_lossy())
                } else {
                    p == path
                }
            })
        };
        let mut resident: Vec<(PathBuf, TextureId)> = self
            .vehicle_textures
            .lock()
            .iter()
            .filter(|(p, _)| same_file(p))
            .map(|(p, (id, _))| (p.clone(), *id))
            .collect();
        resident.extend(
            self.gpu
                .lock()
                .textures
                .iter()
                .filter(|(p, _)| same_file(p))
                .map(|(p, entry)| (p.clone(), entry.id)),
        );
        if resident.is_empty() {
            return Err("texture is not currently resident in the scene".into());
        }
        let image = omsi_texture::decode_file(&path).map_err(|e| e.to_string())?;
        if image.width == 0 || image.height == 0 || image.width > 4096 || image.height > 4096 {
            return Err("refreshed texture dimensions exceed 4096 by 4096".into());
        }
        {
            let paths: hashbrown::HashSet<_> = resident.iter().map(|(p, _)| p.clone()).collect();
            let mut pinned = self.refreshed_texture_paths.lock();
            if pinned.union(&paths).count() > 64 {
                return Err("refreshed file texture limit reached".into());
            }
            // Keep these keys pinned for this World: queued compression may finish later.
            pinned.extend(paths);
        }
        let data = TextureData::from_image(image);
        let mut ids: Vec<_> = resident.iter().map(|(_, id)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        for &id in &ids {
            renderer.replace_texture(scene, id, &data);
        }
        for (path, id) in resident {
            self.textures.release(&path);
            self.vehicle_ready.lock().textures.remove(&path);
            if let Some(entry) = self.gpu.lock().textures.get_mut(&path) {
                entry.bytes = scene.texture_bytes_of(id);
                entry.format = data.format;
                entry.alpha = data.has_alpha;
                entry.texels = data.width as u64 * data.height as u64;
                entry.dropped = 0;
            }
        }
        renderer.rebind_textures(scene, &ids);
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_paths_reject_absolute_parent_and_stream_names() {
        for name in [
            "",
            " ",
            "/outside.png",
            "../outside.png",
            "panel/../../outside.png",
            r"..\outside.png",
            r"C:\outside.png",
            "panel.png:stream",
            r"\\server\panel.png",
        ] {
            assert!(relative_texture_path(name).is_err(), "{name}");
        }
        assert_eq!(
            relative_texture_path(r"display\panel.png").unwrap(),
            Path::new("display/panel.png")
        );
        assert!(relative_texture_path(&"a".repeat(1025)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_paths_do_not_follow_links_outside_vehicle_directories() {
        let root = std::env::temp_dir().join(format!(
            "omsi-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let allowed = root.join("texture");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::write(root.join("outside.png"), b"fixture").unwrap();
        std::fs::write(allowed.join("inside.png"), b"fixture").unwrap();
        std::os::unix::fs::symlink(root.join("outside.png"), allowed.join("escape.png")).unwrap();
        assert!(confined_texture_path(&[allowed.clone()], Path::new("escape.png")).is_none());
        assert!(confined_texture_path(&[allowed], Path::new("inside.png")).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
