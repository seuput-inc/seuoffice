//! Intermediate page representation shared by PDF analysis and Office rebuilds.
use super::{geometry::Rect, script::Script};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    Ltr,
    Rtl,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Alignment {
    Left,
    Center,
    Right,
    Justify,
}
#[derive(Debug, Clone)]
pub(super) struct PdfChar {
    pub code: u32,
    pub text: String,
    pub bounds: Rect,
    pub loose_box: Rect,
    pub origin_x: f64,
    pub origin_y: f64,
    pub angle: f64,
    pub font_size: f64,
    pub font_weight: f64,
    pub font_family: String,
    pub italic: bool,
    pub color: String,
    pub hscale: Option<f64>,
    pub invisible: bool,
    pub is_generated: bool,
    pub pdf_char_index: Option<usize>,
    pub text_object_id: Option<usize>,
    pub is_hyphen: bool,
    pub script: Script,
    pub highlight: Option<String>,
    pub underline: bool,
    pub strike: bool,
    pub note_ref: Option<String>,
    pub href: Option<String>,
}
#[derive(Debug, Clone)]
pub(super) struct Span {
    pub text: String,
    pub bounds: Rect,
    pub font_size: f64,
    pub font_family: String,
    pub bold: bool,
    pub italic: bool,
    pub color: String,
    pub direction: Direction,
    pub script: Script,
    pub highlight: Option<String>,
    pub underline: bool,
    pub strike: bool,
    pub char_scale: Option<f64>,
    pub char_spacing_pt: Option<f64>,
    pub note_ref: Option<String>,
    pub invisible: bool,
    pub href: Option<String>,
}
#[derive(Debug, Clone)]
pub(super) struct Line {
    pub spans: Vec<Span>,
    pub bounds: Rect,
    pub baseline: f64,
    pub ends_with_hyphen: bool,
    pub hard_break_before: bool,
}
#[derive(Debug, Clone)]
pub(super) struct ListInfo {
    pub kind: String,
    pub level: usize,
    pub sequence_id: Option<usize>,
    pub start: Option<i64>,
    pub style: Option<String>,
    pub start_values: Option<Vec<i64>>,
    pub marker: String,
}
#[derive(Debug, Clone)]
pub(super) struct TocEntry {
    pub level: usize,
    pub page_number: String,
}
#[derive(Debug, Clone)]
pub(super) struct DecorBorder {
    pub side: String,
    pub color: String,
    pub width_pt: f64,
    pub space_pt: f64,
    pub indent_right_pt: Option<f64>,
    pub indent_left_pt: Option<f64>,
}
#[derive(Debug, Clone)]
pub(super) struct TextBlock {
    pub lines: Vec<Line>,
    pub bounds: Rect,
    pub align: Alignment,
    pub first_line_indent_pt: f64,
    pub direction: Direction,
    pub list: Option<ListInfo>,
    pub stitched_from_line: Option<usize>,
    pub toc_entry: Option<TocEntry>,
    pub spacing_before_pt: Option<f64>,
    pub border: Option<DecorBorder>,
    pub card_id: Option<usize>,
}
#[derive(Debug, Clone)]
pub(super) struct CardRegion {
    pub bounds: Rect,
    pub color: String,
    pub rounded: bool,
    pub z: Option<i64>,
}
#[derive(Debug, Clone)]
pub(super) struct FloatPlacement {
    pub wrap: String,
    pub x_offset_pt: f64,
}
#[derive(Debug, Clone)]
pub(super) struct ImageBlock {
    pub bounds: Rect,
    pub data: Vec<u8>,
    pub mime: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub float: Option<FloatPlacement>,
    pub spacing_before_pt: Option<f64>,
    pub z: Option<i64>,
    pub synthetic: bool,
    pub card_id: Option<usize>,
}
#[derive(Debug, Clone)]
pub(super) struct Stroke {
    pub bounds: Rect,
    pub orientation: String,
    pub width_pt: f64,
    pub color: String,
    pub from_form: bool,
}
#[derive(Debug, Clone)]
pub(super) struct Fill {
    pub bounds: Rect,
    pub color: String,
    pub alpha: Option<u8>,
    pub z: Option<i64>,
    pub sequence: Option<usize>,
    pub geometry: Option<String>,
    pub corner_radius_pt: Option<f64>,
}
#[derive(Debug, Clone)]
pub(super) struct PageShapes {
    pub strokes: Vec<Stroke>,
    pub fills: Vec<Fill>,
    pub ignored_paths: usize,
    pub curved_fills: Vec<Fill>,
}
#[derive(Debug, Clone)]
pub(super) struct RawSubpath {
    pub points: Vec<(f64, f64)>,
    pub closed: bool,
    pub has_curves: bool,
    pub line_to: Option<Vec<bool>>,
}
#[derive(Debug, Clone)]
pub(super) struct RawPath {
    pub subpaths: Vec<RawSubpath>,
    pub filled: bool,
    pub stroked: bool,
    pub fill_color: String,
    pub stroke_color: String,
    pub stroke_width: f64,
    pub fill_alpha: Option<u8>,
    pub from_form: bool,
    pub z: Option<i64>,
    pub clip_box: Option<Rect>,
}
#[derive(Debug, Clone, Default)]
pub(super) struct SoftEdges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}
#[derive(Debug, Clone)]
pub(super) struct TableCell {
    pub bounds: Rect,
    pub grid_span: usize,
    pub v_merge: Option<String>,
    pub fill: Option<String>,
    pub v_align: Option<String>,
    pub soft_edges: Option<SoftEdges>,
    pub blocks: Vec<TextBlock>,
}
#[derive(Debug, Clone)]
pub(super) struct TableBlock {
    pub bounds: Rect,
    pub col_widths_pt: Vec<f64>,
    pub rows: Vec<Vec<TableCell>>,
    pub confidence: Option<f64>,
    pub border_color: Option<String>,
    pub form: bool,
    pub separator_rule: Option<String>,
    pub spacing_before_pt: Option<f64>,
}
#[derive(Debug, Clone)]
pub(super) enum PageBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Table(TableBlock),
}
#[derive(Debug, Clone)]
pub(super) struct Footnote {
    pub id: String,
    pub marker: Option<String>,
    pub blocks: Vec<TextBlock>,
}
#[derive(Debug, Clone)]
pub(super) struct PageRender {
    pub data: Vec<u8>,
    pub mime: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
}
#[derive(Debug, Clone)]
pub(super) struct PageColumn {
    pub bounds: Rect,
    pub blocks: Vec<PageBlock>,
}
#[derive(Debug, Clone)]
pub(super) struct PageSection {
    pub bounds: Rect,
    pub columns: Vec<PageColumn>,
    pub gutter_widths_pt: Vec<f64>,
    pub direction: Direction,
    pub column_separator: bool,
}
#[derive(Debug, Clone, Default)]
pub(super) struct IrPage {
    pub index: usize,
    pub flows_from_previous: bool,
    pub width_pt: f64,
    pub height_pt: f64,
    pub rotation: i32,
    pub blocks: Vec<PageBlock>,
    pub degraded: bool,
    pub scanned: bool,
    pub ocr_recovered: bool,
    pub has_struct_tree: bool,
    pub degraded_reason: Option<String>,
    pub render: Option<PageRender>,
    pub background_color: Option<String>,
    pub background_render: Option<PageRender>,
    pub background_panels: Vec<ImageBlock>,
    pub cards: Vec<CardRegion>,
    pub shapes: Option<PageShapes>,
    pub sections: Vec<PageSection>,
    pub warnings: Vec<String>,
    pub confidence: Option<f64>,
    pub footnotes: Vec<Footnote>,
    pub ignored_vertical_decor: usize,
    pub canvas: bool,
    pub decor_images: Vec<ImageBlock>,
}
#[derive(Debug, Clone)]
pub(super) struct ExtractedPage {
    pub index: usize,
    pub width_pt: f64,
    pub height_pt: f64,
    pub rotation: i32,
    pub chars: Vec<PdfChar>,
    pub images: Vec<ImageBlock>,
    pub paths: Vec<RawPath>,
    pub degraded: bool,
    pub degraded_reason: Option<String>,
    pub scanned: bool,
    pub has_struct_tree: bool,
    pub render: Option<PageRender>,
    pub ocr_text_recovered: bool,
    pub text_lost: bool,
    pub cell_data: bool,
    pub bad_unicode_ratio: f64,
    pub background_render: Option<PageRender>,
    pub content_box: Option<Rect>,
    pub decor_images: Vec<ImageBlock>,
}

#[derive(Debug, Clone)]
pub(super) struct FurnitureHf {
    pub band: String,
    pub text: String,
    pub page_no: bool,
    pub font_size_pt: f64,
    pub font_family: String,
    pub bold: bool,
    pub italic: bool,
    pub color: String,
    pub x0: f64,
    pub x1: f64,
    pub edge_dist_pt: f64,
    pub covers_first_page: bool,
}
