use crate::scan::{NodeId, Tree};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl Area {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }

    fn shorter_side(&self) -> f32 {
        self.width.min(self.height)
    }

    fn shrunk(&self, margin: f32) -> Area {
        Area {
            left: self.left + margin,
            top: self.top + margin,
            width: (self.width - 2.0 * margin).max(0.0),
            height: (self.height - 2.0 * margin).max(0.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub node: NodeId,
    pub area: Area,
    pub depth: u16,
    /// Painted as one block: a file, or a folder too small on screen to subdivide.
    pub is_leaf: bool,
}

const SMALLEST_SUBDIVIDED_SIDE: f32 = 10.0;
const SMALLEST_VISIBLE_PIXELS: f32 = 6.0;
const FOLDER_MARGIN: f32 = 1.0;

/// Squarified treemap of everything below `root`. Folders come before their contents, so the
/// last tile containing a point is the deepest one.
pub fn layout(tree: &Tree, root: NodeId, area: Area) -> Vec<Tile> {
    let mut tiles = Vec::new();
    place(tree, root, area, 0, &mut tiles);
    tiles
}

fn place(tree: &Tree, node: NodeId, area: Area, depth: u16, tiles: &mut Vec<Tile>) {
    let subdivides = tree.is_directory(node)
        && area.shorter_side() >= SMALLEST_SUBDIVIDED_SIDE
        && tree.node(node).size > 0;
    tiles.push(Tile { node, area, depth, is_leaf: !subdivides });
    if !subdivides {
        return;
    }

    let inner = area.shrunk(FOLDER_MARGIN);
    let total_bytes = tree.node(node).size as f64;
    let pixels_per_byte = f64::from(inner.width * inner.height) / total_bytes;
    let mut children: Vec<(NodeId, f32)> = tree
        .children(node)
        .map(|child| (child, (tree.node(child).size as f64 * pixels_per_byte) as f32))
        .filter(|&(_, pixels)| pixels >= SMALLEST_VISIBLE_PIXELS)
        .collect();
    children.sort_by(|left, right| right.1.total_cmp(&left.1));

    let pixels: Vec<f32> = children.iter().map(|&(_, pixels)| pixels).collect();
    for (&(child, _), child_area) in children.iter().zip(squarify(&pixels, inner)) {
        place(tree, child, child_area, depth + 1, tiles);
    }
}

/// Bruls, Huizing and van Wijk: fill the area row by row along its shorter side, growing a row
/// only while that keeps its tiles closer to square. `pixels` must be sorted largest first.
pub fn squarify(pixels: &[f32], area: Area) -> Vec<Area> {
    let mut placed = Vec::with_capacity(pixels.len());
    let mut remaining = area;
    let mut row_start = 0;
    while row_start < pixels.len() {
        let side = remaining.shorter_side().max(f32::EPSILON);
        let largest = pixels[row_start];
        let mut row_end = row_start + 1;
        let mut row_pixels = largest;
        let mut worst = worst_aspect_ratio(row_pixels, largest, largest, side);
        while row_end < pixels.len() {
            let next = pixels[row_end];
            let grown = worst_aspect_ratio(row_pixels + next, largest, next, side);
            if grown > worst {
                break;
            }
            worst = grown;
            row_pixels += next;
            row_end += 1;
        }

        let thickness = row_pixels / side;
        let lays_out_vertically = remaining.width >= remaining.height;
        let mut offset = 0.0;
        for &tile_pixels in &pixels[row_start..row_end] {
            let length = tile_pixels / thickness.max(f32::EPSILON);
            placed.push(if lays_out_vertically {
                Area {
                    left: remaining.left,
                    top: remaining.top + offset,
                    width: thickness,
                    height: length,
                }
            } else {
                Area {
                    left: remaining.left + offset,
                    top: remaining.top,
                    width: length,
                    height: thickness,
                }
            });
            offset += length;
        }
        if lays_out_vertically {
            remaining.left += thickness;
            remaining.width = (remaining.width - thickness).max(0.0);
        } else {
            remaining.top += thickness;
            remaining.height = (remaining.height - thickness).max(0.0);
        }
        row_start = row_end;
    }
    placed
}

fn worst_aspect_ratio(row_pixels: f32, largest: f32, smallest: f32, side: f32) -> f32 {
    let side_squared = side * side;
    let row_squared = row_pixels * row_pixels;
    (side_squared * largest / row_squared).max(row_squared / (side_squared * smallest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANVAS: Area = Area { left: 10.0, top: 20.0, width: 600.0, height: 400.0 };

    #[test]
    fn tiles_cover_the_area_exactly_and_stay_inside_it() {
        let pixels = [96_000.0, 60_000.0, 48_000.0, 24_000.0, 12_000.0];

        let placed = squarify(&pixels, CANVAS);

        let covered: f32 = placed.iter().map(|area| area.width * area.height).sum();
        assert!((covered - 240_000.0).abs() < 1.0, "covered {covered}");
        for area in &placed {
            assert!(area.left >= CANVAS.left - 0.01 && area.top >= CANVAS.top - 0.01);
            assert!(area.left + area.width <= CANVAS.left + CANVAS.width + 0.01);
            assert!(area.top + area.height <= CANVAS.top + CANVAS.height + 0.01);
        }
    }

    #[test]
    fn each_tile_gets_pixels_in_proportion_to_its_weight() {
        let pixels = [120_000.0, 80_000.0, 40_000.0];

        let placed = squarify(&pixels, CANVAS);

        for (area, expected) in placed.iter().zip(pixels) {
            assert!((area.width * area.height - expected).abs() < 1.0);
        }
    }

    #[test]
    fn equal_weights_stay_close_to_square() {
        let pixels = [60_000.0; 4];

        let placed = squarify(&pixels, CANVAS);

        for area in placed {
            let ratio = (area.width / area.height).max(area.height / area.width);
            assert!(ratio < 2.0, "ratio {ratio}");
        }
    }
}
