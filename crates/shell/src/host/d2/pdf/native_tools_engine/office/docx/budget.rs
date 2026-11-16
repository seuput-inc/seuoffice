//! Reconcile measured PDF whitespace with Word's per-page flow budget.
use super::super::ir::{IrPage, PageBlock, PageSection};
use super::geometry::Geometry;
use super::{card, flow};
use std::collections::HashSet;

pub(super) struct Budget {
    pub usable: f64,
    pub height_scale: f64,
    pub spacing_scale: f64,
    pub lead: f64,
    pub pay_surplus: bool,
    pub pin_columns: bool,
}

pub(super) fn compute(page: &IrPage, sections: &[PageSection], geo: &Geometry) -> Budget {
    let first = sections
        .first()
        .and_then(|s| s.columns.first())
        .and_then(|c| c.blocks.iter().find(|b| !flow::is_float(b)));
    let mut lead = if page.flows_from_previous {
        0.0
    } else {
        first.map_or(0.0, |b| {
            (page.height_pt - geo.margin_top_pt - flow::bounds(b).y1).max(0.0)
        })
    };
    if lead < 2.0 {
        lead = 0.0;
    }
    let notes: f64 = page
        .footnotes
        .iter()
        .flat_map(|n| &n.blocks)
        .map(|b| b.bounds.height())
        .sum();
    let usable = (page.height_pt
        - geo.margin_top_pt
        - geo.margin_bottom_pt
        - 48.0
        - if notes > 0.0 { notes + 12.0 } else { 0.0 })
    .max(0.0);
    let pin_count = usize::from(page.background_render.is_some())
        + page
            .background_panels
            .iter()
            .filter(|p| p.card_id.is_none())
            .count()
        + sections
            .iter()
            .flat_map(|s| &s.columns)
            .flat_map(|c| &c.blocks)
            .filter(|b| flow::is_float(b))
            .count();
    let mut heights = if pin_count > 0 { 1.0 } else { 0.0 };
    let (mut want, mut display_risk, mut body_risk, mut card_pad) =
        (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    let mut seen = HashSet::new();
    for s in sections {
        let (mut tallest, mut with_want) = (0.0_f64, 0.0_f64);
        for c in &s.columns {
            let (mut h, mut gap) = (0.0, 0.0);
            for b in &c.blocks {
                if flow::is_float(b) {
                    continue;
                }
                if let (Some(card), PageBlock::Text(t)) = (card::card_of(b, page), b) {
                    let id = t.card_id.unwrap();
                    if seen.insert(id) {
                        h += card.bounds.height();
                        gap += (card::spacing(t, card) - card_pad).max(0.0);
                        let min_y = page
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                PageBlock::Text(t) if t.card_id == Some(id) => Some(t.bounds.y0),
                                _ => None,
                            })
                            .fold(f64::INFINITY, f64::min);
                        card_pad = (min_y - card.bounds.y0).max(0.0);
                    }
                    continue;
                }
                let bh = flow::height(b, geo, 1.0);
                h += bh;
                gap += (flow::spacing(b) - card_pad).max(0.0);
                card_pad = 0.0;
                if let PageBlock::Text(t) = b {
                    if t.lines.len() >= 2 && flow::display_risk(t, c.bounds.width()) {
                        display_risk += bh / t.lines.len() as f64;
                    } else if t.lines.len() >= 2 && s.columns.len() == 1 && flow::body_risk(t, geo)
                    {
                        body_risk += bh / t.lines.len().max(1) as f64;
                    }
                }
            }
            tallest = tallest.max(h);
            with_want = with_want.max(h + gap);
        }
        heights += tallest;
        want += (with_want - tallest).max(0.0);
    }
    heights += display_risk;
    body_risk = body_risk.min(40.0);
    let has_table = sections
        .iter()
        .flat_map(|s| &s.columns)
        .flat_map(|c| &c.blocks)
        .any(|b| matches!(b, PageBlock::Table(_)));
    let micro = if has_table {
        0
    } else {
        sections
            .iter()
            .filter(|s| flow::micro_section(s, geo))
            .count()
    };
    let chrome = if micro >= 2 {
        (micro as f64 * 16.0).min(48.0)
    } else {
        0.0
    };
    let height_scale = if heights > usable {
        ((usable - 36.0) / heights).max(0.55)
    } else {
        1.0
    };
    let scaled = heights * height_scale;
    let lead_fund = lead.min((usable - scaled).max(0.0));
    if want <= 0.0 || usable - scaled - chrome - lead_fund >= want {
        body_risk = 0.0;
    }
    let spacing_scale = if want > 0.0 {
        ((usable - scaled - body_risk - chrome - lead_fund) / want).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let surplus: f64 = sections
        .iter()
        .filter(|s| s.columns.len() == 1)
        .flat_map(|s| &s.columns[0].blocks)
        .map(flow::surplus)
        .sum();
    let pay_surplus = usable - scaled - body_risk - chrome - lead_fund - want < surplus + 96.0;
    let mut fit_cards = HashSet::new();
    let fit = sections.iter().all(|s| {
        s.columns.iter().all(|c| {
            let mut total = 0.0;
            for b in &c.blocks {
                if flow::is_float(b) {
                    continue;
                }
                if let (Some(card), PageBlock::Text(t)) = (card::card_of(b, page), b) {
                    if fit_cards.insert(t.card_id.unwrap()) {
                        total += card.bounds.height() * height_scale
                            + card::spacing(t, card) * spacing_scale;
                    }
                    continue;
                }
                let h = flow::height(b, geo, height_scale);
                total += h + flow::spacing(b) * spacing_scale;
                if let PageBlock::Text(t) = b
                    && !t.lines.is_empty()
                    && flow::display_risk(t, c.bounds.width())
                {
                    total += h / t.lines.len() as f64;
                }
            }
            total <= usable + 24.0 - 36.0
        })
    });
    let behind = sections
        .iter()
        .flat_map(|s| &s.columns)
        .flat_map(|c| &c.blocks)
        .all(|b| match b {
            PageBlock::Image(image) if image.float.is_some() => {
                image.float.as_ref().is_some_and(|f| {
                    f.wrap == "behind"
                        || page.width_pt > page.height_pt
                        || image.bounds.height().max(1.0) >= page.height_pt * 0.6
                })
            }
            _ => true,
        });
    Budget {
        usable,
        height_scale,
        spacing_scale,
        lead: lead_fund,
        pay_surplus,
        pin_columns: fit && behind,
    }
}
