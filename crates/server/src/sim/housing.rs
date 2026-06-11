//! Housing validity (DESIGN §7.1): the pure room checker and the
//! scan-on-demand house search.
//!
//! A house is validated by flood-filling from a candidate interior air cell.
//! The fill is 8-connected and blocked by solid tiles (closed doors are
//! solid), and by platforms (§7.1) — so a platform can seal a wall hole, and
//! an *open* door leaks the fill (rooms are evaluated with doors shut; NPC
//! pass-through closes doors behind itself).
//!
//! Search strategy (documented per the §7.1 "checked on demand + every dawn"
//! rule): the world is never scanned wholesale. At dawn and whenever an NPC
//! needs a home, candidate interior cells are enumerated in expanding boxes
//! around the town center (the housed NPCs' homes, else the world spawn),
//! radii 100 → 200 → 400 tiles. A shared visited set means each connected
//! air region floods at most once per search (the open sky burns its 750-cell
//! budget once and is skipped thereafter), keeping a search to a handful of
//! bounded fills.

use std::collections::HashSet;

use ferraria_shared::tiles::{TileId, WallId};
use ferraria_shared::world::World;

/// Rule 1: the fill must terminate within this many interior cells.
pub const HOUSE_FILL_CAP: usize = 750;
/// Rule 1: the room must stay this many tiles away from the world edge.
pub const HOUSE_EDGE_MARGIN: u32 = 10;
/// Rule 2: total size (interior + boundary frame) bounds.
pub const HOUSE_MIN_TILES: usize = 60;
pub const HOUSE_MAX_TILES: usize = 750;
/// Rule 5: fraction of interior cells that need a background wall.
pub const HOUSE_WALL_COVERAGE_PERCENT: usize = 60;

/// Search radii (tiles) around the town center, tried in order.
pub const HOUSE_SEARCH_RADII: [u32; 3] = [100, 200, 400];

/// Which §7.1 rule a candidate room failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HousingError {
    /// The start cell isn't interior air.
    BadStart,
    /// Rule 1: flood fill exceeded [`HOUSE_FILL_CAP`] cells.
    FillTooLarge,
    /// Rule 1: the room touches a tile within 10 tiles of the world edge.
    TouchesWorldEdge,
    /// Rule 2: interior + frame under 60 tiles.
    TooSmall,
    /// Rule 2: interior + frame over 750 tiles.
    TooLarge,
    /// Rule 3: no door in the boundary.
    NoDoor,
    /// Rule 4: no light source (Torch/Furnace) inside.
    NoLight,
    /// Rule 4: no flat-surface item (Table/Workbench) inside.
    NoFlatSurface,
    /// Rule 4: no comfort item (Chair/Bed) inside.
    NoComfort,
    /// Rule 5: under 60% of interior cells have a background wall.
    LowWallCoverage,
    /// Rule 6: no 1×3 air column standing on solid floor free of doors.
    NoHomeTile,
}

/// A validated room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct House {
    /// Every interior (non-blocking) cell the fill reached.
    pub interior: Vec<(u32, u32)>,
    /// The chosen §7.1 home tile: the bottom cell of a 1×3 air column on
    /// solid floor (where the NPC stands).
    pub home: (u32, u32),
}

impl House {
    pub fn contains(&self, cell: (u32, u32)) -> bool {
        self.interior.contains(&cell)
    }
}

/// Does this cell block the §7.1 flood fill? Solid tiles (closed doors are
/// solid) and platforms do; all other non-solid content (furniture, torches,
/// pots, open doors) is interior.
fn blocks_fill(world: &World, x: u32, y: u32) -> bool {
    let t = world.tile(x, y);
    t.is_solid() || t.is_platform()
}

/// Validates the room containing `start` against §7.1 rules 1–6 (rule 7 —
/// no other NPC assigned to the room — needs the NPC roster and is checked
/// by the caller). On success also records the fill in `visited` so a wider
/// search never re-floods the same region; failed fills record too.
pub fn check_house_visited(
    world: &World,
    start: (u32, u32),
    visited: &mut HashSet<(u32, u32)>,
) -> Result<House, HousingError> {
    let (sx, sy) = start;
    if !world.in_bounds(sx, sy) || blocks_fill(world, sx, sy) {
        return Err(HousingError::BadStart);
    }

    // ---- Flood fill (rule 1). -------------------------------------------------
    let mut interior: Vec<(u32, u32)> = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let mut stack = vec![(sx, sy)];
    seen.insert((sx, sy));
    let mut overflow = false;
    let mut touches_edge = false;
    while let Some((x, y)) = stack.pop() {
        interior.push((x, y));
        if interior.len() > HOUSE_FILL_CAP {
            overflow = true;
            break;
        }
        if x < HOUSE_EDGE_MARGIN
            || y < HOUSE_EDGE_MARGIN
            || x + HOUSE_EDGE_MARGIN >= world.width
            || y + HOUSE_EDGE_MARGIN >= world.height
        {
            touches_edge = true;
            break;
        }
        // 8-connectivity.
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 {
                    touches_edge = true; // off-world is trivially near the edge
                    continue;
                }
                let (nx, ny) = (nx as u32, ny as u32);
                if !world.in_bounds(nx, ny) {
                    touches_edge = true;
                    continue;
                }
                if blocks_fill(world, nx, ny) || !seen.insert((nx, ny)) {
                    continue;
                }
                stack.push((nx, ny));
            }
        }
    }
    visited.extend(seen.iter().copied());
    if overflow {
        return Err(HousingError::FillTooLarge);
    }
    if touches_edge {
        return Err(HousingError::TouchesWorldEdge);
    }

    // ---- Boundary frame: blocking cells 8-adjacent to the interior. ------------
    let mut frame: HashSet<(u32, u32)> = HashSet::new();
    let mut has_door = false;
    for &(x, y) in &interior {
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 {
                    continue;
                }
                let (nx, ny) = (nx as u32, ny as u32);
                if world.in_bounds(nx, ny)
                    && blocks_fill(world, nx, ny)
                    && frame.insert((nx, ny))
                    && world.tile(nx, ny).id == TileId::Door
                {
                    has_door = true;
                }
            }
        }
    }

    // ---- Rule 2: total size bounds. ---------------------------------------------
    let total = interior.len() + frame.len();
    if total < HOUSE_MIN_TILES {
        return Err(HousingError::TooSmall);
    }
    if total > HOUSE_MAX_TILES {
        return Err(HousingError::TooLarge);
    }

    // ---- Rule 3: a door in the boundary. ------------------------------------------
    if !has_door {
        return Err(HousingError::NoDoor);
    }

    // ---- Rule 4: light / flat surface / comfort inside. -----------------------------
    let mut light = false;
    let mut flat = false;
    let mut comfort = false;
    for &(x, y) in &interior {
        match world.tile(x, y).id {
            TileId::Torch | TileId::Furnace => light = true,
            TileId::Table | TileId::Workbench => flat = true,
            TileId::Chair | TileId::Bed => comfort = true,
            _ => {}
        }
    }
    if !light {
        return Err(HousingError::NoLight);
    }
    if !flat {
        return Err(HousingError::NoFlatSurface);
    }
    if !comfort {
        return Err(HousingError::NoComfort);
    }

    // ---- Rule 5: ≥60% wall coverage of the interior. -------------------------------
    let walled = interior
        .iter()
        .filter(|&&(x, y)| world.tile(x, y).wall != WallId::Air)
        .count();
    if walled * 100 < interior.len() * HOUSE_WALL_COVERAGE_PERCENT {
        return Err(HousingError::LowWallCoverage);
    }

    // ---- Rule 6: a home tile — 1×3 air column on solid floor, no door. ---------------
    let cells: HashSet<(u32, u32)> = interior.iter().copied().collect();
    let home = interior
        .iter()
        .copied()
        .filter(|&(x, y)| is_home_tile(world, &cells, x, y))
        // Deterministic pick: bottom-most, then left-most.
        .max_by_key(|&(x, y)| (y, std::cmp::Reverse(x)));
    let Some(home) = home else {
        return Err(HousingError::NoHomeTile);
    };

    Ok(House { interior, home })
}

/// Convenience wrapper without a shared visited set.
pub fn check_house(world: &World, start: (u32, u32)) -> Result<House, HousingError> {
    check_house_visited(world, start, &mut HashSet::new())
}

/// Rule 6: `(x, y)` is the feet cell of a 1×3 in-room air column standing on
/// solid floor, with no door occupying any column cell.
fn is_home_tile(world: &World, interior: &HashSet<(u32, u32)>, x: u32, y: u32) -> bool {
    if y < 2 || !world.is_solid(x as i32, y as i32 + 1) {
        return false;
    }
    (0..3u32).all(|d| {
        let cy = y - d;
        interior.contains(&(x, cy)) && world.tile(x, cy).id == TileId::Air
    })
}

/// Scan-on-demand search (see module docs): the nearest valid house around
/// `center` whose room contains none of `occupied_homes` (§7.1 rule 7).
/// Returns the best match by home-tile distance to `center`.
pub fn find_vacant_house(
    world: &World,
    center: (u32, u32),
    occupied_homes: &[(u32, u32)],
) -> Option<House> {
    let mut visited: HashSet<(u32, u32)> = HashSet::new();
    let mut best: Option<(u64, House)> = None;
    for radius in HOUSE_SEARCH_RADII {
        let x0 = center.0.saturating_sub(radius);
        let x1 = (center.0 + radius).min(world.width.saturating_sub(1));
        let y0 = center.1.saturating_sub(radius);
        let y1 = (center.1 + radius).min(world.height.saturating_sub(1));
        for y in y0..=y1 {
            for x in x0..=x1 {
                if visited.contains(&(x, y)) {
                    continue;
                }
                // Candidates: air cells with solid directly below (a
                // standable floor) — every real room contains one, and it
                // prunes the vast solid underground.
                if world.tile(x, y).id != TileId::Air || !world.is_solid(x as i32, y as i32 + 1) {
                    continue;
                }
                let Ok(house) = check_house_visited(world, (x, y), &mut visited) else {
                    continue;
                };
                if occupied_homes.iter().any(|&h| house.contains(h)) {
                    continue; // rule 7: someone already lives here
                }
                let (dx, dy) = (
                    house.home.0 as i64 - center.0 as i64,
                    house.home.1 as i64 - center.1 as i64,
                );
                let d = (dx * dx + dy * dy) as u64;
                if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, house));
                }
            }
        }
        if let Some((_, house)) = best {
            return Some(house);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferraria_shared::tiles::{state, Tile};

    /// A 200×200 all-air world with a solid stone slab from row 100 down —
    /// room floors sit on the slab, comfortably clear of all edges.
    const FLOOR: u32 = 100;

    fn base_world() -> World {
        let mut w = World::new(200, 200);
        for y in FLOOR..200 {
            for x in 0..200 {
                w.set_tile(x, y, Tile::of(TileId::Stone));
            }
        }
        w
    }

    fn set(w: &mut World, x: u32, y: u32, id: TileId) {
        let mut t = w.tile(x, y);
        t.id = id;
        t.state = 0;
        w.set_tile(x, y, t);
    }

    /// Builds a §7.1-valid room whose interior is `iw`×`ih` with its
    /// bottom-left interior cell at (x0, FLOOR-1): stone shell, wood walls
    /// behind every interior cell, a closed door mid-left-wall, torch,
    /// table, chair. Returns an interior start cell.
    fn build_room(w: &mut World, x0: u32, iw: u32, ih: u32) -> (u32, u32) {
        let y_top = FLOOR - ih; // top interior row
        for x in x0 - 1..=x0 + iw {
            for y in y_top - 1..=FLOOR {
                let interior =
                    (x0..x0 + iw).contains(&x) && (y_top..y_top + ih).contains(&y) && y < FLOOR;
                let mut t = w.tile(x, y);
                if interior {
                    t.id = TileId::Air;
                    t.wall = WallId::Wood;
                } else {
                    t.id = TileId::Stone;
                }
                w.set_tile(x, y, t);
            }
        }
        // Door: 1×3 in the left wall, feet at floor level.
        assert!(ih >= 3, "room tall enough for a door");
        for d in 0..3u32 {
            let mut t = w.tile(x0 - 1, FLOOR - 1 - d);
            t.id = TileId::Door;
            t.state = state::part(0, (2 - d) as u8);
            w.set_tile(x0 - 1, FLOOR - 1 - d, t);
        }
        // Furniture along the floor: torch, table (3×2), chair (1×2).
        set(w, x0, FLOOR - 1, TileId::Torch);
        assert!(w.place_multitile(x0 + 1, FLOOR - 2, TileId::Table));
        assert!(w.place_multitile(x0 + 4, FLOOR - 2, TileId::Chair));
        (x0 + 5, FLOOR - 1)
    }

    /// Minimal valid room: 10×4 interior (40) + frame (12×6 − 40 = 32)
    /// = 72 ≥ 60 total. (An 8×3 interior + frame lands under 60.)
    fn valid_room(w: &mut World) -> (u32, u32) {
        build_room(w, 50, 10, 4)
    }

    #[test]
    fn minimal_valid_room_passes_and_picks_a_home_tile() {
        let mut w = base_world();
        let start = valid_room(&mut w);
        let house = check_house(&w, start).expect("valid room");
        // 10×4 interior.
        assert_eq!(house.interior.len(), 40);
        // Home tile: air column on the floor, away from the door column.
        let (hx, hy) = house.home;
        assert_eq!(hy, FLOOR - 1);
        assert!(w.is_solid(hx as i32, FLOOR as i32));
        for d in 0..3 {
            assert_eq!(w.tile(hx, hy - d).id, TileId::Air);
        }
    }

    #[test]
    fn room_below_60_total_tiles_is_too_small() {
        let mut w = base_world();
        // 7×3 interior (21) + frame (9×5 − 21 = 24) = 45 < 60.
        let start = build_room(&mut w, 50, 7, 3);
        assert_eq!(check_house(&w, start), Err(HousingError::TooSmall));
    }

    #[test]
    fn unenclosed_fill_overflows_or_touches_edge() {
        let w = base_world();
        // Open air above the slab: the fill spreads unbounded.
        let err = check_house(&w, (100, FLOOR - 1)).expect_err("open air");
        assert!(
            matches!(
                err,
                HousingError::FillTooLarge | HousingError::TouchesWorldEdge
            ),
            "{err:?}"
        );
    }

    #[test]
    fn rooms_near_the_world_edge_are_rejected() {
        let mut w = base_world();
        // A valid room shape but hugging the left world edge (x0 = 5 < 10).
        let start = build_room(&mut w, 6, 10, 4);
        assert_eq!(check_house(&w, start), Err(HousingError::TouchesWorldEdge));
    }

    #[test]
    fn missing_door_fails_rule_3() {
        let mut w = base_world();
        let start = valid_room(&mut w);
        // Brick up the door.
        for d in 1..=3u32 {
            set(&mut w, 49, FLOOR - d, TileId::Stone);
        }
        assert_eq!(check_house(&w, start), Err(HousingError::NoDoor));
    }

    #[test]
    fn closed_door_is_boundary_not_interior() {
        let mut w = base_world();
        let start = valid_room(&mut w);
        let house = check_house(&w, start).expect("valid");
        // The fill never passed through the closed door into the outside.
        assert!(!house.contains((48, FLOOR - 1)), "outside the door");
        assert!(!house.contains((49, FLOOR - 1)), "the door cell itself");
    }

    #[test]
    fn platform_in_boundary_blocks_the_leak() {
        let mut w = base_world();
        let start = valid_room(&mut w);
        // Punch a ceiling hole and seal it with a platform: still a room.
        set(&mut w, 52, FLOOR - 5, TileId::Platform);
        let before = check_house(&w, start).expect("platform seals the hole");
        assert!(!before.contains((52, FLOOR - 5)), "platform is boundary");
        // Remove the platform: the room leaks out the hole and dies.
        set(&mut w, 52, FLOOR - 5, TileId::Air);
        let err = check_house(&w, start).expect_err("leaks");
        assert!(
            matches!(
                err,
                HousingError::FillTooLarge | HousingError::TouchesWorldEdge
            ),
            "{err:?}"
        );
    }

    #[test]
    fn furniture_requirements_each_fail_individually() {
        // No light.
        let mut w = base_world();
        let start = valid_room(&mut w);
        set(&mut w, 50, FLOOR - 1, TileId::Air); // remove torch
        assert_eq!(check_house(&w, start), Err(HousingError::NoLight));

        // No flat surface (clear the 3×2 table).
        let mut w = base_world();
        let start = valid_room(&mut w);
        for dx in 0..3 {
            for dy in 0..2 {
                set(&mut w, 51 + dx, FLOOR - 2 + dy, TileId::Air);
            }
        }
        assert_eq!(check_house(&w, start), Err(HousingError::NoFlatSurface));

        // No comfort (clear the 1×2 chair).
        let mut w = base_world();
        let start = valid_room(&mut w);
        for dy in 0..2 {
            set(&mut w, 54, FLOOR - 2 + dy, TileId::Air);
        }
        assert_eq!(check_house(&w, start), Err(HousingError::NoComfort));

        // A furnace counts as light, a workbench as flat, a bed as comfort.
        let mut w = base_world();
        let start = build_room(&mut w, 50, 12, 4);
        set(&mut w, 50, FLOOR - 1, TileId::Air); // strip default torch
        for dx in 0..3 {
            for dy in 0..2 {
                set(&mut w, 51 + dx, FLOOR - 2 + dy, TileId::Air); // table
            }
        }
        for dy in 0..2 {
            set(&mut w, 54, FLOOR - 2 + dy, TileId::Air); // chair
        }
        assert!(w.place_multitile(50, FLOOR - 2, TileId::Furnace));
        assert!(w.place_multitile(53, FLOOR - 1, TileId::Workbench));
        assert!(w.place_multitile(55, FLOOR - 2, TileId::Bed));
        check_house(&w, start).expect("furnace/workbench/bed satisfy rule 4");
    }

    #[test]
    fn wall_coverage_edge_at_exactly_60_percent() {
        let mut w = base_world();
        let start = valid_room(&mut w);
        // 40 interior cells. Strip walls from 16 → 24/40 = 60%: passes.
        let house = check_house(&w, start).expect("valid");
        let cells = house.interior.clone();
        for &(x, y) in cells.iter().take(16) {
            let mut t = w.tile(x, y);
            t.wall = WallId::Air;
            w.set_tile(x, y, t);
        }
        check_house(&w, start).expect("exactly 60% coverage passes");
        // One more stripped → 23/40 = 57.5%: fails rule 5.
        let &(x, y) = &cells[16];
        let mut t = w.tile(x, y);
        t.wall = WallId::Air;
        w.set_tile(x, y, t);
        assert_eq!(check_house(&w, start), Err(HousingError::LowWallCoverage));
    }

    #[test]
    fn home_tile_needs_a_clear_1x3_column_on_solid_floor() {
        let mut w = base_world();
        // A wide but 2-tall interior: no 3-high column exists. Interior
        // 30×2 = 60 cells + frame 32×4 − 60 = 68 → 128 total, in bounds.
        let y_top = FLOOR - 2;
        for x in 49..=80u32 {
            for y in y_top - 1..=FLOOR {
                let interior = (50..80).contains(&x) && y >= y_top && y < FLOOR;
                let mut t = w.tile(x, y);
                if interior {
                    t.id = TileId::Air;
                    t.wall = WallId::Wood;
                } else {
                    t.id = TileId::Stone;
                }
                w.set_tile(x, y, t);
            }
        }
        // Door can't be 1×3 in a 2-tall wall; cheat one into the ceiling
        // boundary so rule 3 passes and rule 6 is what fails.
        set(&mut w, 60, y_top - 1, TileId::Door);
        set(&mut w, 50, FLOOR - 1, TileId::Torch);
        assert!(w.place_multitile(52, FLOOR - 1, TileId::Workbench));
        assert!(w.place_multitile(55, FLOOR - 2, TileId::Bed));
        assert_eq!(
            check_house(&w, (70, FLOOR - 1)),
            Err(HousingError::NoHomeTile)
        );
    }

    #[test]
    fn fill_cap_is_750_cells() {
        let mut w = base_world();
        // A 100×8 sealed hall = 800 interior cells > 750.
        let y_top = FLOOR - 8;
        for x in 39..=140u32 {
            for y in y_top - 1..=FLOOR {
                let interior = (40..140).contains(&x) && y >= y_top && y < FLOOR;
                let mut t = w.tile(x, y);
                if interior {
                    t.id = TileId::Air;
                    t.wall = WallId::Wood;
                } else {
                    t.id = TileId::Stone;
                }
                w.set_tile(x, y, t);
            }
        }
        assert_eq!(
            check_house(&w, (90, FLOOR - 1)),
            Err(HousingError::FillTooLarge)
        );
    }

    #[test]
    fn find_vacant_house_returns_nearest_and_respects_occupancy() {
        let mut w = base_world();
        let near = build_room(&mut w, 90, 10, 4);
        let far = build_room(&mut w, 130, 10, 4);
        let center = (100, FLOOR - 1);
        let house = find_vacant_house(&w, center, &[]).expect("found");
        let near_home = check_house(&w, near).expect("near valid").home;
        let far_home = check_house(&w, far).expect("far valid").home;
        assert_eq!(house.home, near_home, "nearest house wins");
        // Occupy the near one: the far one is returned instead (rule 7).
        let house = find_vacant_house(&w, center, &[near_home]).expect("found far");
        assert_eq!(house.home, far_home);
        // Both occupied: nothing.
        assert!(find_vacant_house(&w, center, &[near_home, far_home]).is_none());
    }
}
