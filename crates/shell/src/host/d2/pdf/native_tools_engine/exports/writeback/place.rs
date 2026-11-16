//! Collision-aware placement of translated PDF blocks.
use super::layout::{self, Align, Layout, LayoutOptions, Measurer, Rect};
use std::collections::HashMap;

pub(super) struct PlanBlock<'a> {
    pub key: String,
    pub text: String,
    pub boxes: Vec<Rect>,
    pub pages: Vec<usize>,
    pub size: f64,
    pub line_height: f64,
    pub align: Align,
    pub font: &'a dyn Measurer,
    pub group: String,
    pub expandable: bool,
    pub container: Option<Rect>,
    pub alternatives: Vec<Vec<Rect>>,
}
pub(super) struct PageSpace {
    pub text: Vec<Rect>,
    pub shapes: Vec<Rect>,
    pub content: Rect,
}
#[derive(Clone)]
pub(super) struct Placement {
    pub layout: Layout,
    pub boxes: Vec<Rect>,
}
struct Surroundings {
    obstacles: Vec<Rect>,
    containers: Vec<Rect>,
}

fn intersects(a: Rect, b: Rect, tol: f64) -> bool {
    a.x0 < b.x1 - tol && a.x1 > b.x0 + tol && a.y0 < b.y1 - tol && a.y1 > b.y0 + tol
}
fn contains(a: Rect, b: Rect, tol: f64) -> bool {
    a.x0 <= b.x0 + tol && a.y0 <= b.y0 + tol && a.x1 >= b.x1 - tol && a.y1 >= b.y1 - tol
}
pub(super) fn ink_of(layout: &Layout, font: &dyn Measurer) -> Vec<Rect> {
    let up = font.ascent().min(0.8) * layout.size;
    let down = font.descent().min(0.15) * layout.size;
    layout
        .lines
        .iter()
        .map(|line| Rect {
            x0: line.x,
            y0: line.y - down,
            x1: line.x + font.measure(&line.text) * layout.size,
            y1: line.y + up,
        })
        .collect()
}
fn surroundings(
    own: Rect,
    solid: &[Rect],
    shapes: &[Rect],
    drawn: &[Rect],
    content: Option<Rect>,
    size: f64,
) -> Surroundings {
    let reach = Rect {
        x0: own.x0 - size,
        y0: own.y0.min(content.map_or(own.y0, |c| c.y0)) - size,
        x1: own.x1.max(content.map_or(own.x1, |c| c.x1)) + size,
        y1: own.y1 + size,
    };
    let mut obstacles = Vec::new();
    let mut containers = Vec::new();
    for r in solid {
        if !intersects(*r, own, 0.0) && intersects(*r, reach, 0.0) {
            obstacles.push(*r);
        }
    }
    for r in drawn {
        if intersects(*r, reach, 0.0) {
            obstacles.push(*r);
        }
    }
    for r in shapes {
        if contains(*r, own, 1.0) {
            containers.push(*r);
        } else if !intersects(*r, own, 0.0) && intersects(*r, reach, 0.0) {
            obstacles.push(*r);
        }
    }
    Surroundings {
        obstacles,
        containers,
    }
}
fn grown_boxes(b: Rect, around: &Surroundings, content: Rect, size: f64) -> Vec<Rect> {
    let mut y0 = b.y0.min(content.y0);
    for ob in &around.obstacles {
        if ob.x0 < b.x1 && ob.x1 > b.x0 && ob.y1 <= b.y0 + 0.5 {
            y0 = y0.max(ob.y1 + 1.0_f64.max(0.2 * size));
        }
    }
    for c in &around.containers {
        y0 = y0.max(c.y0 + 1.0);
    }
    let down = Rect {
        y0: y0.min(b.y0),
        ..b
    };
    let mut x1 = b.x1.max(content.x1);
    for ob in &around.obstacles {
        if ob.y0 < down.y1 && ob.y1 > down.y0 && ob.x0 >= b.x1 - 0.5 {
            x1 = x1.min(ob.x0 - 3.0_f64.max(0.5 * size));
        }
    }
    for c in &around.containers {
        x1 = x1.min(c.x1 - 1.0);
    }
    let both = Rect {
        x1: x1.max(b.x1),
        ..down
    };
    let mut result = Vec::new();
    if down.y0 < b.y0 - 0.5 {
        result.push(down);
    }
    if both.x1 > b.x1 + 0.5 {
        result.push(both);
    }
    result
}
fn grown_around(b: Rect, around: &Surroundings, size: f64) -> Vec<Rect> {
    let gap = 2.0_f64.max(0.2 * size);
    let mut left = f64::NEG_INFINITY;
    let mut right = f64::INFINITY;
    for ob in &around.obstacles {
        if ob.y0 >= b.y1 || ob.y1 <= b.y0 {
            continue;
        }
        if ob.x1 <= b.x0 + 0.5 {
            left = left.max(ob.x1 + gap);
        }
        if ob.x0 >= b.x1 - 0.5 {
            right = right.min(ob.x0 - gap);
        }
    }
    for c in &around.containers {
        left = left.max(c.x0 + 1.0);
        right = right.min(c.x1 - 1.0);
    }
    let half = 0.0_f64.max((b.x0 - left).min(right - b.x1));
    let wide = Rect {
        x0: b.x0 - half,
        x1: b.x1 + half,
        ..b
    };
    let mut y0 = f64::NEG_INFINITY;
    for ob in &around.obstacles {
        if ob.x0 < wide.x1 && ob.x1 > wide.x0 && ob.y1 <= b.y0 + 0.5 {
            y0 = y0.max(ob.y1 + gap);
        }
    }
    for c in &around.containers {
        y0 = y0.max(c.y0 + 1.0);
    }
    let tall = Rect {
        y0: b.y0.min(y0),
        ..wide
    };
    let mut result = Vec::new();
    if half > 0.5 {
        result.push(wide);
    }
    if tall.y0 < b.y0 - 0.5 {
        result.push(tall);
    }
    result
}
fn try_at(
    block: &PlanBlock<'_>,
    around: &[Surroundings],
    boxes: &[Rect],
    scale: f64,
) -> Result<Option<Layout>, String> {
    let layout = layout::layout_text(
        &block.text,
        boxes,
        block.size * scale,
        block.font,
        LayoutOptions {
            line_height: block.line_height,
            align: block.align,
        },
    )?;
    if layout.overflowed {
        return Ok(None);
    }
    let ink = ink_of(&layout, block.font);
    for (i, line) in layout.lines.iter().enumerate() {
        let nearby = around
            .get(line.box_index)
            .ok_or("Placement box has no page surroundings.")?;
        if nearby
            .obstacles
            .iter()
            .any(|ob| intersects(ink[i], *ob, 0.25))
            || nearby
                .containers
                .iter()
                .any(|container| !contains(*container, ink[i], 0.25))
        {
            return Ok(None);
        }
    }
    Ok(Some(layout))
}
fn first_fit(
    block: &PlanBlock<'_>,
    around: &[Surroundings],
    steps: &[f64],
    from: usize,
    content: Option<Rect>,
) -> Result<Option<(usize, Placement)>, String> {
    let mut grown = None;
    for (k, scale) in steps.iter().copied().enumerate().skip(from) {
        if let Some(layout) = try_at(block, around, &block.boxes, scale)? {
            return Ok(Some((
                k,
                Placement {
                    layout,
                    boxes: block.boxes.clone(),
                },
            )));
        }
        for boxes in &block.alternatives {
            if let Some(layout) = try_at(block, around, boxes, scale)? {
                return Ok(Some((
                    k,
                    Placement {
                        layout,
                        boxes: boxes.clone(),
                    },
                )));
            }
        }
        if !block.expandable || content.is_none() || scale >= 0.9 {
            continue;
        }
        if grown.is_none() {
            let b = *block.boxes.first().ok_or("Expandable block has no box.")?;
            let surrounding = around
                .first()
                .ok_or("Expandable block has no surroundings.")?;
            grown = Some(if block.container.is_some() {
                grown_around(b, surrounding, block.size)
            } else {
                grown_boxes(
                    b,
                    surrounding,
                    content.ok_or("Expandable block has no content bounds.")?,
                    block.size,
                )
            });
        }
        if let Some(boxes) = &grown {
            for b in boxes {
                if let Some(layout) = try_at(block, around, &[*b], scale)? {
                    return Ok(Some((
                        k,
                        Placement {
                            layout,
                            boxes: vec![*b],
                        },
                    )));
                }
            }
        }
    }
    Ok(None)
}
fn near(
    block: &PlanBlock<'_>,
    index: usize,
    blocks: &[PlanBlock<'_>],
    spaces: &HashMap<usize, PageSpace>,
    drawn: &[Rect],
) -> Result<Surroundings, String> {
    let page = *block.pages.get(index).ok_or("Placement box has no page.")?;
    let own = *block.boxes.get(index).ok_or("Placement box is missing.")?;
    let mut solid = spaces
        .get(&page)
        .map_or_else(Vec::new, |space| space.text.clone());
    for other in blocks {
        if std::ptr::eq(other, block) {
            continue;
        }
        solid.extend(
            other
                .boxes
                .iter()
                .zip(&other.pages)
                .filter_map(|(r, p)| (*p == page).then_some(*r)),
        );
    }
    let mut shapes = spaces
        .get(&page)
        .map_or_else(Vec::new, |space| space.shapes.clone());
    if let Some(container) = block.container {
        shapes.push(container);
    }
    let content = if index == 0 && block.expandable {
        block
            .container
            .or_else(|| spaces.get(&page).map(|s| s.content))
    } else {
        None
    };
    Ok(surroundings(
        own, &solid, &shapes, drawn, content, block.size,
    ))
}
pub(super) fn place_blocks(
    blocks: &[PlanBlock<'_>],
    spaces: &HashMap<usize, PageSpace>,
    floor: f64,
) -> Result<HashMap<String, Option<Placement>>, String> {
    let steps = layout::scale_steps(floor);
    let mut best = HashMap::new();
    for block in blocks {
        let around = (0..block.boxes.len())
            .map(|i| near(block, i, blocks, spaces, &[]))
            .collect::<Result<Vec<_>, _>>()?;
        let content = block.container.or_else(|| {
            block
                .pages
                .first()
                .and_then(|page| spaces.get(page).map(|s| s.content))
        });
        if let Some((step, _)) = first_fit(block, &around, &steps, 0, content)? {
            best.insert(block.key.clone(), step);
        }
    }
    let mut weights: HashMap<&str, HashMap<usize, usize>> = HashMap::new();
    for block in blocks {
        if let Some(step) = best.get(&block.key) {
            *weights
                .entry(&block.group)
                .or_default()
                .entry(*step)
                .or_default() += block.text.chars().count();
        }
    }
    let shared: HashMap<_, _> = weights
        .into_iter()
        .filter_map(|(group, weights)| {
            weights
                .into_iter()
                .max_by_key(|(step, weight)| (*weight, *step))
                .map(|(step, _)| (group, step))
        })
        .collect();
    let mut claims: HashMap<usize, Vec<Rect>> = HashMap::new();
    let mut out = HashMap::new();
    for block in blocks {
        let Some(mine) = best.get(&block.key).copied() else {
            out.insert(block.key.clone(), None);
            continue;
        };
        let around = (0..block.boxes.len())
            .map(|i| {
                let page = block
                    .pages
                    .get(i)
                    .copied()
                    .ok_or("Placement box has no page.")?;
                near(
                    block,
                    i,
                    blocks,
                    spaces,
                    claims.get(&page).map(Vec::as_slice).unwrap_or(&[]),
                )
            })
            .collect::<Result<Vec<_>, String>>()?;
        let content = block.container.or_else(|| {
            block
                .pages
                .first()
                .and_then(|page| spaces.get(page).map(|s| s.content))
        });
        let from = mine.max(shared.get(block.group.as_str()).copied().unwrap_or(mine));
        let placement =
            first_fit(block, &around, &steps, from, content)?.map(|(_, placement)| placement);
        if let Some(placement) = &placement {
            for (line, ink) in placement
                .layout
                .lines
                .iter()
                .zip(ink_of(&placement.layout, block.font))
            {
                let page = *block
                    .pages
                    .get(line.box_index)
                    .ok_or("Placed line has no page.")?;
                claims.entry(page).or_default().push(ink);
            }
        }
        out.insert(block.key.clone(), placement);
    }
    Ok(out)
}
