//! Height-aware cutter footprints against the original, uncut terrain triangles.
use super::{hole_mesh_rims, outline_crosses_itself, tile_size, DVec2, DVec3, Mat4, MeshData, Terrain, Vec3};
use std::collections::HashSet;

/// Coplanar surfaces, including sub-millimetre placement noise, keep their terrain.
pub const TERRAIN_HOLE_EPSILON: f64 = 0.001;
const AREA_EPS: f64 = 1e-10;

fn area(a: DVec3, b: DVec3, c: DVec3) -> f64 {
    (b - a).truncate().perp_dot((c - a).truncate())
}

fn valid_terrain(terrain: &Terrain) -> bool {
    terrain.cells > 0
        && terrain.cells.checked_add(1).and_then(|n| n.checked_mul(n)) == Some(terrain.heights.len())
        && terrain.heights.iter().all(|h| h.is_finite())
        && tile_size().is_finite()
        && tile_size() > 0.0
}

/// Clip a convex polygon by a linear half-plane, interpolating cutter height too.
fn clip(polygon: Vec<DVec3>, distance: impl Fn(DVec3) -> f64) -> Vec<DVec3> {
    let Some(&last) = polygon.last() else { return polygon };
    let mut out = Vec::with_capacity(polygon.len() + 1);
    let (mut a, mut da) = (last, distance(last));
    for b in polygon {
        let db = distance(b);
        if (da >= 0.0) != (db >= 0.0) {
            out.push(a.lerp(b, da / (da - db)));
        }
        if db >= 0.0 {
            out.push(b);
        }
        (a, da) = (b, db);
    }
    out.dedup_by(|a, b| a.distance_squared(*b) < AREA_EPS * AREA_EPS);
    if out.len() > 1 && out[0].distance_squared(*out.last().unwrap()) < AREA_EPS * AREA_EPS {
        out.pop();
    }
    out
}

fn append_triangle(mesh: &mut MeshData, p: [DVec3; 3]) {
    if area(p[0], p[1], p[2]).abs() <= AREA_EPS {
        return;
    }
    let drawn = p.map(|v| v.as_vec3());
    if area(drawn[0].as_dvec3(), drawn[1].as_dvec3(), drawn[2].as_dvec3()).abs() <= AREA_EPS {
        return;
    }
    let base = mesh.positions.len() as u32;
    mesh.positions.extend(drawn);
    mesh.normals.extend([Vec3::Z; 3]);
    mesh.uvs.extend(p.map(|v| (v.truncate() / tile_size()).as_vec2()));
    mesh.indices.extend([base, base + 1, base + 2]);
}

fn below_triangles(triangles: impl IntoIterator<Item = [DVec3; 3]>, terrain: &Terrain) -> MeshData {
    let mut mesh = MeshData::default();
    if !valid_terrain(terrain) {
        return mesh;
    }
    let side = tile_size();
    let cell = side / terrain.cells as f64;
    for cutter in triangles {
        if cutter.iter().any(|p| !p.is_finite()) || area(cutter[0], cutter[1], cutter[2]).abs() <= AREA_EPS {
            continue;
        }
        let lo = cutter.iter().fold(DVec2::splat(f64::INFINITY), |a, p| a.min(p.truncate()));
        let hi = cutter.iter().fold(DVec2::splat(f64::NEG_INFINITY), |a, p| a.max(p.truncate()));
        if hi.x < 0.0 || hi.y < 0.0 || lo.x > side || lo.y > side {
            continue;
        }
        let bound = |v: f64| ((v / cell).floor().max(0.0) as usize).min(terrain.cells - 1);
        for j in bound(lo.y)..=bound(hi.y) {
            for i in bound(lo.x)..=bound(hi.x) {
                let at = |x: usize, y: usize| DVec3::new(x as f64 * cell, y as f64 * cell, terrain.height_at(x, y) as f64);
                let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
                // Match Terrain::sample and the original drawn grid's 00 -> 11 diagonal.
                for ground in [[a, b, c], [a, c, d]] {
                    let mut polygon = cutter.to_vec();
                    for edge in 0..3 {
                        let (u, v) = (ground[edge], ground[(edge + 1) % 3]);
                        polygon = clip(polygon, |p| area(u, v, p));
                    }
                    if polygon.len() < 3 {
                        continue;
                    }
                    let [a, b, c] = ground;
                    let det = area(a, b, c);
                    let clearance = |p: DVec3| {
                        let wb = (p - a).truncate().perp_dot((c - a).truncate()) / det;
                        let wc = (b - a).truncate().perp_dot((p - a).truncate()) / det;
                        a.z + (b.z - a.z) * wb + (c.z - a.z) * wc - p.z - TERRAIN_HOLE_EPSILON
                    };
                    polygon = clip(polygon, clearance);
                    for k in 1..polygon.len().saturating_sub(1) {
                        append_triangle(&mut mesh, [polygon[0], polygon[k], polygon[k + 1]]);
                    }
                }
            }
        }
    }
    if !mesh.indices.is_empty() {
        mesh.ranges.push((0, mesh.indices.len() as u32, 0));
    }
    mesh
}

/// Triangulate a simple outline without replacing authored heights by an average plane.
fn outline_triangles(rim: &[DVec3]) -> Vec<[DVec3; 3]> {
    if rim.iter().any(|p| !p.is_finite()) {
        return Vec::new();
    }
    let mut points: Vec<DVec3> = Vec::with_capacity(rim.len());
    for &p in rim {
        if let Some(last) = points.last_mut().filter(|q| q.truncate().distance_squared(p.truncate()) < AREA_EPS * AREA_EPS) {
            // A vertical rim segment has no projected width; its lower endpoint owns it.
            last.z = last.z.min(p.z);
        } else {
            points.push(p);
        }
    }
    if points.len() > 1 && points[0].truncate().distance_squared(points.last().unwrap().truncate()) < AREA_EPS * AREA_EPS {
        points[0].z = points[0].z.min(points.pop().unwrap().z);
    }
    let xy: Vec<_> = points.iter().map(|p| p.truncate()).collect();
    if points.len() < 3 || outline_crosses_itself(&xy) {
        return Vec::new();
    }
    let signed_area: f64 = (0..points.len()).map(|i| xy[i].perp_dot(xy[(i + 1) % xy.len()])).sum();
    if signed_area.abs() <= AREA_EPS {
        return Vec::new();
    }
    if signed_area < 0.0 {
        points.reverse();
    }
    let mut order: Vec<_> = (0..points.len()).collect();
    let mut out = Vec::with_capacity(points.len() - 2);
    while order.len() > 3 {
        let ear = (0..order.len()).find(|&k| {
            let [a, b, c] = [points[order[(k + order.len() - 1) % order.len()]], points[order[k]], points[order[(k + 1) % order.len()]]];
            area(a, b, c) > AREA_EPS && order.iter().all(|&v| {
                [a, b, c].contains(&points[v]) || area(a, b, points[v]) < -AREA_EPS || area(b, c, points[v]) < -AREA_EPS || area(c, a, points[v]) < -AREA_EPS
            })
        });
        let Some(k) = ear else { return Vec::new() };
        out.push([points[order[(k + order.len() - 1) % order.len()]], points[order[k]], points[order[(k + 1) % order.len()]]]);
        order.remove(k);
    }
    out.push([points[order[0]], points[order[1]], points[order[2]]]);
    out
}

/// Keep only an object's cutter footprint lying at least 1 mm below ORIGINAL terrain.
/// Inputs share world coordinates; output positions are local to `terrain_origin`.
/// The returned mesh is a cutter union, never rendered side walls. Open rims bounded
/// entirely by vertical faces supply their interior with their own authored height.
pub fn mesh_hole_below_terrain(mesh: &MeshData, transform: &Mat4, origin: DVec3, terrain: &Terrain, terrain_origin: DVec3) -> MeshData {
    if !origin.is_finite() || !terrain_origin.is_finite() || !transform.is_finite()
        || mesh.positions.iter().any(|p| !p.is_finite())
        || mesh.indices.iter().any(|&i| i as usize >= mesh.positions.len())
    {
        return MeshData::default();
    }
    let offset = origin - terrain_origin;
    let mut triangles: Vec<_> = mesh.indices.chunks_exact(3).map(|t| {
        [0, 1, 2].map(|k| transform.transform_point3(mesh.positions[t[k] as usize]).as_dvec3() + offset)
    }).collect();
    let key = |p: DVec3| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64, (p.z * 1000.0).round() as i64);
    for rim in hole_mesh_rims(mesh, transform, offset) {
        let rim_keys: HashSet<_> = rim.iter().map(|&p| key(p)).collect();
        let has_surface = triangles.iter().any(|t| area(t[0], t[1], t[2]).abs() > AREA_EPS && t.iter().filter(|&&p| rim_keys.contains(&key(p))).count() >= 2);
        if !has_surface {
            triangles.extend(outline_triangles(&rim));
        }
    }
    below_triangles(triangles, terrain)
}

/// Keep only an authored 3D cutter outline footprint below ORIGINAL terrain.
/// Concave outlines retain their shape and vertex heights. Output is tile-local to
/// `terrain_origin`; no terrain query ever uses an already cut or flattened mesh.
pub fn outline_hole_below_terrain(rim_world: &[DVec3], terrain: &Terrain, terrain_origin: DVec3) -> MeshData {
    if !terrain_origin.is_finite() {
        return MeshData::default();
    }
    let local: Vec<_> = rim_world.iter().map(|p| *p - terrain_origin).collect();
    below_triangles(outline_triangles(&local), terrain)
}

/// Clip ACTUAL spline triangles to the hole's projected outline, then below original
/// terrain. Outline heights are intentionally absent: generated hole profiles contain
/// a lowered floor which is not a rendered spline surface. Interior profile/station
/// breaks and each rendered triangle's height survive both XY and height clipping.
/// All input origins are world coordinates; output is local to `terrain_origin`.
pub fn mesh_hole_below_terrain_in_outline(mesh: &MeshData, transform: &Mat4, origin: DVec3, outline_world: &[DVec2], terrain: &Terrain, terrain_origin: DVec3) -> MeshData {
    if !origin.is_finite() || !terrain_origin.is_finite() || !transform.is_finite()
        || mesh.positions.iter().any(|p| !p.is_finite())
        || mesh.indices.iter().any(|&i| i as usize >= mesh.positions.len())
    {
        return MeshData::default();
    }
    let outline: Vec<_> = outline_world.iter().map(|p| (*p - terrain_origin.truncate()).extend(0.0)).collect();
    let footprint = outline_triangles(&outline);
    let bounds = |p: &[DVec3; 3]| (
        p.iter().fold(DVec2::splat(f64::INFINITY), |a, p| a.min(p.truncate())),
        p.iter().fold(DVec2::splat(f64::NEG_INFINITY), |a, p| a.max(p.truncate())),
    );
    let footprint: Vec<_> = footprint.into_iter().map(|p| (p, bounds(&p))).collect();
    let offset = origin - terrain_origin;
    let mut fragments = Vec::new();
    for t in mesh.indices.chunks_exact(3) {
        let cutter = [0, 1, 2].map(|k| transform.transform_point3(mesh.positions[t[k] as usize]).as_dvec3() + offset);
        if area(cutter[0], cutter[1], cutter[2]).abs() <= AREA_EPS {
            continue;
        }
        let (lo, hi) = bounds(&cutter);
        for (shape, (a, b)) in &footprint {
            if hi.x < a.x || hi.y < a.y || lo.x > b.x || lo.y > b.y {
                continue;
            }
            let mut polygon = cutter.to_vec();
            for edge in 0..3 {
                polygon = clip(polygon, |p| area(shape[edge], shape[(edge + 1) % 3], p));
            }
            for k in 1..polygon.len().saturating_sub(1) {
                fragments.push([polygon[0], polygon[k], polygon[k + 1]]);
            }
        }
    }
    below_triangles(fragments, terrain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_spline_mesh, spline_hole_outlines, SplineCurve};
    use omsi_scenery::sli::{Spline, SplineProfile, SplineProfilePoint};

    fn flat(height: f32) -> Terrain { Terrain { cells: 1, heights: vec![height; 4] } }
    fn quad(z: f32, side: f32) -> MeshData {
        MeshData { positions: vec![Vec3::new(0.0, 0.0, z), Vec3::new(side, 0.0, z), Vec3::new(side, side, z), Vec3::new(0.0, side, z)], indices: vec![0, 1, 2, 0, 2, 3], ..Default::default() }
    }
    fn projected_area(mesh: &MeshData) -> f64 {
        mesh.indices.chunks_exact(3).map(|t| {
            let p = [0, 1, 2].map(|k| mesh.positions[t[k] as usize].as_dvec3());
            area(p[0], p[1], p[2]).abs() * 0.5
        }).sum()
    }
    fn covers(mesh: &MeshData, p: DVec2) -> bool {
        mesh.indices.chunks_exact(3).any(|t| {
            let [a, b, c] = [0, 1, 2].map(|k| mesh.positions[t[k] as usize].as_dvec3());
            let s = area(a, b, c).signum();
            [area(a, b, p.extend(0.0)), area(b, c, p.extend(0.0)), area(c, a, p.extend(0.0))].iter().all(|v| v * s >= -1e-5)
        })
    }

    #[test]
    fn above_coplanar_and_submillimetre_cutters_keep_terrain() {
        for z in [11.0, 10.0, 9.9995] {
            let mesh = quad(z, 20.0);
            let cut = mesh_hole_below_terrain(&mesh, &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::ZERO);
            assert!(cut.indices.is_empty(), "z={z}");
            let rim: Vec<_> = mesh.positions.iter().map(|p| p.as_dvec3()).collect();
            assert!(outline_hole_below_terrain(&rim, &flat(10.0), DVec3::ZERO).indices.is_empty(), "spline z={z}");
        }
        let cut = mesh_hole_below_terrain(&quad(9.998, 20.0), &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::ZERO);
        assert!((projected_area(&cut) - 400.0).abs() < 1e-4);
    }

    #[test]
    fn intersecting_triangle_interpolates_only_below_grade() {
        let mesh = MeshData { positions: vec![Vec3::new(10.0, 10.0, 9.0), Vec3::new(20.0, 10.0, 11.0), Vec3::new(10.0, 20.0, 9.0)], indices: vec![0, 1, 2], ..Default::default() };
        let cut = mesh_hole_below_terrain(&mesh, &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::ZERO);
        let width = (1.0 - TERRAIN_HOLE_EPSILON) / 0.2;
        assert!((projected_area(&cut) - (10.0 * width - width * width / 2.0)).abs() < 1e-4);
        assert!(covers(&cut, DVec2::new(12.0, 12.0)));
        assert!(!covers(&cut, DVec2::new(17.0, 11.0)));
        assert!(cut.positions.iter().all(|p| p.x <= (10.0 + width) as f32 + 1e-5));
        assert!(cut.positions.iter().all(|p| p.z <= 10.0 - TERRAIN_HOLE_EPSILON as f32 + 1e-5));
        let rim: Vec<_> = mesh.positions.iter().map(|p| p.as_dvec3()).collect();
        let spline = outline_hole_below_terrain(&rim, &flat(10.0), DVec3::ZERO);
        assert!((projected_area(&spline) - projected_area(&cut)).abs() < 1e-4);
        assert!(!covers(&spline, DVec2::new(17.0, 11.0)));
    }

    #[test]
    fn original_grid_peak_cuts_even_when_all_cutter_corners_are_above_ground() {
        let side = tile_size();
        let terrain = Terrain { cells: 2, heights: vec![0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0] };
        let before = terrain.clone();
        let cut = mesh_hole_below_terrain(&quad(5.0, side as f32), &Mat4::IDENTITY, DVec3::ZERO, &terrain, DVec3::ZERO);
        let fraction = (5.0 - TERRAIN_HOLE_EPSILON) / 10.0;
        assert!((projected_area(&cut) - 6.0 * side * side / 8.0 * fraction * fraction).abs() < 0.02);
        assert!(covers(&cut, DVec2::splat(side * 0.5)));
        assert!(!covers(&cut, DVec2::splat(side * 0.1)));
        assert_eq!(terrain, before, "queries must not modify original terrain");
    }

    #[test]
    fn object_transform_and_spline_outline_share_large_world_coordinates() {
        let terrain_origin = DVec3::new(7_000_000.25, 9_000_000.5, 250.0);
        let origin = terrain_origin + DVec3::new(30.0, 40.0, 9.0);
        let mesh = quad(0.0, 10.0);
        let transform = Mat4::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let object = mesh_hole_below_terrain(&mesh, &transform, origin, &flat(10.0), terrain_origin);
        let rim: Vec<_> = mesh.positions.iter().map(|&p| origin + transform.transform_point3(p).as_dvec3()).collect();
        let spline = outline_hole_below_terrain(&rim, &flat(10.0), terrain_origin);
        assert!((projected_area(&object) - 100.0).abs() < 1e-3);
        assert!((projected_area(&object) - projected_area(&spline)).abs() < 1e-3);
        for cut in [object, spline] {
            assert!(covers(&cut, DVec2::new(25.0, 45.0)));
            assert!(cut.positions.iter().all(|p| (p.z - 9.0).abs() < 1e-5));
        }
    }

    #[test]
    fn concave_outline_keeps_its_uncovered_notch() {
        let rim: Vec<_> = [(10.0, 10.0), (30.0, 10.0), (30.0, 20.0), (20.0, 20.0), (20.0, 30.0), (10.0, 30.0)].into_iter().map(|(x, y)| DVec3::new(x, y, 9.0)).collect();
        let cut = outline_hole_below_terrain(&rim, &flat(10.0), DVec3::ZERO);
        assert!((projected_area(&cut) - 300.0).abs() < 1e-4);
        assert!(covers(&cut, DVec2::new(15.0, 25.0)));
        assert!(!covers(&cut, DVec2::new(25.0, 25.0)));
        let reversed: Vec<_> = rim.into_iter().rev().collect();
        assert!((projected_area(&outline_hole_below_terrain(&reversed, &flat(10.0), DVec3::ZERO)) - 300.0).abs() < 1e-4);
    }

    #[test]
    fn vertical_open_cutter_rims_supply_height_aware_interior() {
        let mut walls = MeshData::default();
        for (a, b) in [(Vec3::new(10.0, 10.0, 0.0), Vec3::new(20.0, 10.0, 0.0)), (Vec3::new(20.0, 10.0, 0.0), Vec3::new(20.0, 20.0, 0.0)), (Vec3::new(20.0, 20.0, 0.0), Vec3::new(10.0, 20.0, 0.0)), (Vec3::new(10.0, 20.0, 0.0), Vec3::new(10.0, 10.0, 0.0))] {
            let base = walls.positions.len() as u32;
            walls.positions.extend([a + Vec3::Z * 8.0, b + Vec3::Z * 8.0, b + Vec3::Z * 9.0, a + Vec3::Z * 9.0]);
            walls.indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        let below = mesh_hole_below_terrain(&walls, &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::ZERO);
        assert!(covers(&below, DVec2::new(15.0, 15.0)));
        assert!(!covers(&below, DVec2::new(25.0, 15.0)));
        assert!(mesh_hole_below_terrain(&walls, &Mat4::IDENTITY, DVec3::new(0.0, 0.0, 3.0), &flat(10.0), DVec3::ZERO).indices.is_empty());
    }

    #[test]
    fn invalid_sources_do_not_create_a_cut() {
        let invalid = Terrain { cells: 2, heights: vec![0.0; 4] };
        assert!(mesh_hole_below_terrain(&quad(-1.0, 20.0), &Mat4::IDENTITY, DVec3::ZERO, &invalid, DVec3::ZERO).indices.is_empty());
        let mut bad = quad(-1.0, 20.0);
        bad.indices.push(99);
        assert!(mesh_hole_below_terrain(&bad, &Mat4::IDENTITY, DVec3::ZERO, &flat(0.0), DVec3::ZERO).indices.is_empty());
        let crossing = [DVec3::new(10.0, 10.0, -1.0), DVec3::new(20.0, 20.0, -1.0), DVec3::new(10.0, 20.0, -1.0), DVec3::new(20.0, 10.0, -1.0)];
        assert!(outline_hole_below_terrain(&crossing, &flat(0.0), DVec3::ZERO).indices.is_empty());
    }

    fn curve(height: f64) -> SplineCurve {
        SplineCurve { start: DVec3::new(100.0, 100.0, height), heading_deg: 0.0, length: 20.0, radius: 0.0, grad_start: 0.0, grad_end: 0.0, delta_h: None, cant_start: 0.0, cant_end: 0.0, skew_start: 0.0, skew_end: 0.0, tex_offset: 0.0, seed: 0, half_cant_width: 10.0 }
    }
    fn spline_definition() -> Spline {
        Spline { profiles: vec![SplineProfile { texture: 0, points: vec![SplineProfilePoint { x: -1.0, ..Default::default() }, SplineProfilePoint { x: 3.0, ..Default::default() }] }], ..Default::default() }
    }

    #[test]
    fn actual_spline_height_ignores_synthetic_floor_for_every_mode_and_mirror() {
        for explicit in [false, true] {
            let mut def = spline_definition();
            if explicit {
                // Authored footprint dimensions remain, but these cutter depths do not
                // replace the actual rendered road's height either.
                def.terrain_hole_profiles = vec![vec![[-0.5, -3.0, -0.5], [1.5, -3.0, -0.5]]];
            }
            for mirror in [false, true] {
                for height in [0.05, TERRAIN_HOLE_EPSILON * 0.5, -TERRAIN_HOLE_EPSILON * 0.5, -0.05] {
                    let curve = curve(height);
                    let mesh = build_spline_mesh(&def, &curve, mirror, DVec3::ZERO);
                    for mode in 0..=4 {
                        let mut total = 0.0;
                        for outline in spline_hole_outlines(&def, &curve, mirror, mode) {
                            let cut = mesh_hole_below_terrain_in_outline(&mesh, &Mat4::IDENTITY, DVec3::ZERO, &outline, &flat(0.0), DVec3::ZERO);
                            total += projected_area(&cut);
                        }
                        if mode == 0 || height > -TERRAIN_HOLE_EPSILON {
                            assert_eq!(total, 0.0, "height={height} mode={mode} mirror={mirror} explicit={explicit}");
                        } else {
                            assert!(total > 30.0, "below road must cut its footprint: mode={mode} mirror={mirror} explicit={explicit} area={total}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn actual_spline_gradient_cuts_only_the_buried_end() {
        let def = spline_definition();
        let curve = SplineCurve { grad_start: 10.0, grad_end: 10.0, ..curve(-1.0) };
        let mesh = build_spline_mesh(&def, &curve, false, DVec3::ZERO);
        let outline = spline_hole_outlines(&def, &curve, false, 4).pop().unwrap();
        let cut = mesh_hole_below_terrain_in_outline(&mesh, &Mat4::IDENTITY, DVec3::ZERO, &outline, &flat(0.0), DVec3::ZERO);
        assert!(covers(&cut, DVec2::new(100.5, 105.0)));
        assert!(!covers(&cut, DVec2::new(100.5, 115.0)));
        assert!(cut.positions.iter().all(|p| p.y <= 109.99 + 1e-4));
    }

    #[test]
    fn actual_profile_valley_cuts_interior_even_when_every_outline_height_is_above() {
        let mesh = MeshData { positions: vec![Vec3::new(10.0, 10.0, 1.0), Vec3::new(20.0, 10.0, -1.0), Vec3::new(30.0, 10.0, 1.0), Vec3::new(10.0, 30.0, 1.0), Vec3::new(20.0, 30.0, -1.0), Vec3::new(30.0, 30.0, 1.0)], indices: vec![0, 1, 3, 1, 4, 3, 1, 2, 4, 2, 5, 4], ..Default::default() };
        let outline = [DVec2::new(10.0, 10.0), DVec2::new(30.0, 10.0), DVec2::new(30.0, 30.0), DVec2::new(10.0, 30.0)];
        let cut = mesh_hole_below_terrain_in_outline(&mesh, &Mat4::IDENTITY, DVec3::ZERO, &outline, &flat(0.0), DVec3::ZERO);
        assert!((projected_area(&cut) - 199.8).abs() < 0.001);
        assert!(covers(&cut, DVec2::new(20.0, 20.0)));
        assert!(!covers(&cut, DVec2::new(12.0, 20.0)));
        assert!(!covers(&cut, DVec2::new(28.0, 20.0)));
        let rims = hole_mesh_rims(&cut, &Mat4::IDENTITY, DVec3::ZERO);
        assert_eq!(rims.len(), 1, "clipping subdivisions must not become internal hole boundaries");
        let ring_area = (0..rims[0].len()).map(|i| rims[0][i].truncate().perp_dot(rims[0][(i + 1) % rims[0].len()].truncate())).sum::<f64>().abs() * 0.5;
        assert!((ring_area - projected_area(&cut)).abs() < 0.001);
    }

    #[test]
    fn twisted_grid_diagonal_and_adjacent_tile_seams_use_original_triangles() {
        let side = tile_size();
        let terrain = Terrain { cells: 1, heights: vec![0.0, 10.0, 10.0, 0.0] };
        let cut = mesh_hole_below_terrain(&quad(5.0, side as f32), &Mat4::IDENTITY, DVec3::ZERO, &terrain, DVec3::ZERO);
        let fraction = (5.0 - TERRAIN_HOLE_EPSILON) / 10.0;
        assert!((projected_area(&cut) - side * side * fraction * fraction).abs() < 0.02);
        assert!(!covers(&cut, DVec2::splat(side * 0.5)), "the original 00->11 diagonal is low");
        assert!(covers(&cut, DVec2::new(side * 0.75, side * 0.1)));
        let spanning = MeshData { positions: vec![Vec3::new(side as f32 - 10.0, 10.0, 9.0), Vec3::new(side as f32 + 10.0, 10.0, 9.0), Vec3::new(side as f32 + 10.0, 20.0, 9.0), Vec3::new(side as f32 - 10.0, 20.0, 9.0)], indices: vec![0, 1, 2, 0, 2, 3], ..Default::default() };
        let left = mesh_hole_below_terrain(&spanning, &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::ZERO);
        let right = mesh_hole_below_terrain(&spanning, &Mat4::IDENTITY, DVec3::ZERO, &flat(10.0), DVec3::new(side, 0.0, 0.0));
        assert!((projected_area(&left) - 100.0).abs() < 1e-4);
        assert!((projected_area(&right) - 100.0).abs() < 1e-4);
        assert!(left.positions.iter().any(|p| p.x == side as f32));
        assert!(right.positions.iter().any(|p| p.x == 0.0));
        assert!(left.positions.iter().all(|p| p.x <= side as f32));
        assert!(right.positions.iter().all(|p| p.x >= 0.0));
    }
}
