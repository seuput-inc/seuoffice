//! Native, document-scoped capabilities for presentation media.
//!
//! The archive comes only from the native deck actor. JavaScript can choose a
//! part within that archive, never a host path or another document's archive.
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{Arc, LazyLock, Mutex};

use main_core::slides::jpeg_orientation::neutralize_jpeg_orientation;
use main_core::slides::media_mime::{display_mime, DisplayMime};
use main_core::slides::session_state::retint_svg_rules;
use pptx_engine::{theme, zip::PackageArchive};
use shell_contract::ViewId;
use shell_ipc_d1::slides::engine::DeckId;
use crate::views::SlidesViewLease;

struct Snapshot {
    generation: u64,
    views: HashMap<ViewId, SlidesViewLease>,
    archive: Arc<PackageArchive>,
    first_slide: Option<Arc<str>>,
}

#[derive(Clone)]
struct Source {
    deck: DeckId,
    generation: u64,
    media_ref: String,
    slide_path: Option<String>,
    snapshot: Arc<Snapshot>,
}

#[derive(Default)]
struct Registry {
    decks: HashMap<DeckId, Arc<Snapshot>>,
    sources: HashMap<String, Source>,
    urls: HashMap<(DeckId, u64, String, Option<String>), String>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn registry() -> &'static Mutex<Registry> { &REGISTRY }

/// Publish one native actor snapshot. Old issued URLs survive while their
/// generation is retained for undo; the caller prunes them explicitly.
/// View-set changes require a fresh generation.
pub fn install(
    deck: DeckId,
    generation: u64,
    views: impl IntoIterator<Item = (ViewId, SlidesViewLease)>,
    entries: Vec<(String, Vec<u8>)>,
    first_slide: Option<String>,
) -> Result<(), String> {
    let views: HashMap<_, _> = views.into_iter().collect();
    let mut state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    if views.values().any(|lease| !lease.is_active()) { return Err("slide view lease is not active".into()); }
    if let Some(current) = state.decks.get(&deck) {
        if views.iter().any(|(view, lease)| current.views.get(view).is_some_and(|existing| existing.is_active() && !existing.same_lifetime(lease))) {
            return Err("a different slide view lifetime is already active".into());
        }
        if generation < current.generation {
            return Err("stale slide media generation".into());
        }
        if generation == current.generation {
            return if same_views(&views, &current.views) { Ok(()) } else { Err("slide media grants changed without a new generation".into()) };
        }
    }
    state.decks.insert(deck, Arc::new(Snapshot {
        generation, views, archive: Arc::new(PackageArchive::from_parts(entries, String::new())), first_slide: first_slide.map(Arc::from),
    }));
    Ok(())
}

/// Revoke a deck and every URL issued for it.
pub fn release(deck: DeckId) {
    if let Ok(mut state) = registry().lock() {
        state.decks.remove(&deck);
        state.sources.retain(|_, source| source.deck != deck);
        state.urls.retain(|(id, _, _, _), _| *id != deck);
    }
}
/// Revoke one view lifetime from one deck without affecting other bindings.
/// Retained URLs are gated by the current snapshot, including during reads.
pub fn revoke_deck_view(deck: DeckId, view: ViewId, lease: &SlidesViewLease) -> Result<(), String> {
    let mut state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    let Some(current) = state.decks.get(&deck) else { return Ok(()); };
    if !current.views.get(&view).is_some_and(|stored| stored.same_lifetime(lease)) {
        return Ok(());
    }
    let mut views = HashMap::new();
    views.try_reserve(current.views.len().saturating_sub(1))
        .map_err(|_| "slide media registry allocation failed")?;
    views.extend(current.views.iter().filter(|(id, _)| **id != view).map(|(id, lease)| (*id, lease.clone())));
    let replacement = Arc::new(Snapshot {
        generation: current.generation,
        views,
        archive: current.archive.clone(),
        first_slide: current.first_slide.clone(),
    });
    state.decks.insert(deck, replacement);
    Ok(())
}


/// Revoke only the captured view lifetime, never a replacement with the same ID.
pub fn revoke_view(view: ViewId, lease: &SlidesViewLease) {
    if let Ok(mut state) = registry().lock() {
        let affected: HashSet<_> = state.decks.iter().filter_map(|(deck, snapshot)| {
            let matches = snapshot.views.get(&view).is_some_and(|old| old.same_lifetime(lease));
            (matches && snapshot.views.values().all(|other| !other.is_active())).then_some(*deck)
        }).collect();
        state.decks.retain(|deck, _| !affected.contains(deck));
        state.sources.retain(|_, source| !affected.contains(&source.deck));
        state.urls.retain(|(deck, _, _, _), _| !affected.contains(deck));
    }
}

fn same_views(a: &HashMap<ViewId, SlidesViewLease>, b: &HashMap<ViewId, SlidesViewLease>) -> bool {
    a.len() == b.len() && a.iter().all(|(view, lease)| b.get(view).is_some_and(|other| lease.same_lifetime(other)))
}

fn authorized(snapshot: &Snapshot, view: ViewId, lease: &SlidesViewLease) -> bool {
    lease.is_active() && snapshot.views.get(&view).is_some_and(|stored| stored.same_lifetime(lease))
}

/// Revalidate a callback against its native deck generation and view lifetime.
pub fn authorize(deck: DeckId, generation: u64, view: ViewId, lease: &SlidesViewLease) -> Result<(), String> {
    let state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    let snapshot = state.decks.get(&deck).ok_or("slide media deck unavailable")?;
    if snapshot.generation != generation || !authorized(snapshot, view, lease) {
        return Err("slide media grant is not active".into());
    }
    Ok(())
}

/// Release URLs and snapshots no longer retained by the current deck or undo history.
pub fn prune(deck: DeckId, retained_generations: &[u64]) {
    if let Ok(mut state) = registry().lock() {
        let current = state.decks.get(&deck).map(|snapshot| snapshot.generation);
        let keep = |generation: u64| Some(generation) == current || retained_generations.contains(&generation);
        state.sources.retain(|_, source| source.deck != deck || keep(source.generation));
        state.urls.retain(|(id, generation, _, _), _| *id != deck || keep(*generation));
    }
}

/// Transfer retained undo capabilities before releasing a replaced deck.
/// Generation numbers in the destination must be distinct and unused.
pub fn transfer_history(old: DeckId, new: DeckId, generations: &[(u64, u64)]) -> Result<(), String> {
    if old == new { return Err("cannot transfer slide history to the same deck".into()); }
    let mut state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    let current = state.decks.get(&new).ok_or("destination slide deck unavailable")?.generation;
    transfer_locked(&mut state, old, new, current, generations)
}

fn transfer_locked(state: &mut Registry, old: DeckId, new: DeckId, current: u64, generations: &[(u64, u64)]) -> Result<(), String> {
    // A never-rendered source has issued no capabilities to transfer.
    if !state.decks.contains_key(&old) { return Ok(()); }
    let mut mapping = HashMap::new();
    let mut targets = HashSet::new();
    for &(from, to) in generations {
        if mapping.insert(from, to).is_some() || !targets.insert(to) || to == current
            || state.sources.values().any(|source| source.deck == new && source.generation == to)
        {
            return Err("conflicting slide history generations".into());
        }
    }
    let moved: Vec<_> = state.urls.iter().filter_map(|(key, url)| {
        (key.0 == old).then(|| mapping.get(&key.1).map(|to| (key.clone(), *to, url.clone()))).flatten()
    }).collect();
    for (old_key, generation, url) in moved {
        state.urls.remove(&old_key);
        if let Some(source) = state.sources.get_mut(&url) {
            source.deck = new;
            source.generation = generation;
        }
        state.urls.insert((new, generation, old_key.2, old_key.3), url);
    }
    Ok(())
}

/// Install a replacement and move its retained history as one transaction.
/// Failure leaves both existing decks and all capabilities unchanged.
pub fn install_and_transfer(
    old: DeckId,
    new: DeckId,
    generation: u64,
    views: impl IntoIterator<Item = (ViewId, SlidesViewLease)>,
    entries: Vec<(String, Vec<u8>)>,
    first_slide: Option<String>,
    generations: &[(u64, u64)],
) -> Result<(), String> {
    if old == new { return Err("cannot transfer slide history to the same deck".into()); }
    let snapshot = Arc::new(Snapshot {
        generation,
        views: views.into_iter().collect(),
        archive: Arc::new(PackageArchive::from_parts(entries, String::new())),
        first_slide: first_slide.map(Arc::from),
    });
    let mut state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    if snapshot.views.values().any(|lease| !lease.is_active()) { return Err("slide view lease is not active".into()); }
    if state.decks.contains_key(&new) { return Err("destination slide deck already installed".into()); }
    transfer_locked(&mut state, old, new, generation, generations)?;
    state.decks.insert(new, snapshot);
    Ok(())
}

/// Extract embedded SFNT faces exclusively from an authorized actor snapshot.
pub fn embedded_fonts(
    deck: DeckId, generation: u64, view: ViewId, lease: &SlidesViewLease,
) -> Result<Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>, String> {
    let snapshot = {
        let state = registry().lock().map_err(|_| "slide media registry poisoned")?;
        let snapshot = state.decks.get(&deck).cloned().ok_or("slide media deck unavailable")?;
        if snapshot.generation != generation || !authorized(&snapshot, view, lease) {
            return Err("slide media grant is not active".into());
        }
        snapshot
    };
    let faces = pptx_engine::embedded_fonts::list_embedded_fonts(&snapshot.archive)?;
    let state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    let current = state.decks.get(&deck).ok_or("slide media deck unavailable")?;
    if !Arc::ptr_eq(current, &snapshot) || !authorized(current, view, lease) {
        return Err("slide media grant is not active".into());
    }
    Ok(faces)
}

fn mime_text(mime: DisplayMime) -> &'static str {
    match mime {
        DisplayMime::Mime(mime) => mime,
        DisplayMime::ObjectConstructor => "function Object() { [native code] }",
        DisplayMime::ObjectPrototype => "[object Object]",
    }
}

fn random_uuid() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]))
}

/// Resolve only a part belonging to the authorized native snapshot.
pub fn resolve(
    deck: DeckId,
    generation: u64,
    view: ViewId,
    lease: &SlidesViewLease,
    media_ref: &str,
    slide_path: Option<&str>,
) -> Result<Option<String>, String> {
    let mut state = registry().lock().map_err(|_| "slide media registry poisoned")?;
    let snapshot = state.decks.get(&deck).cloned().ok_or("slide media deck unavailable")?;
    if snapshot.generation != generation || !authorized(&snapshot, view, lease) {
        return Err("slide media grant is not active".into());
    }
    let Some(bytes) = snapshot.archive.read_bytes(media_ref) else { return Ok(None) };
    let mime = mime_text(display_mime(media_ref, bytes));
    let slide_path = slide_path.filter(|s| !s.is_empty()).map(str::to_owned);
    let key = (deck, generation, media_ref.to_owned(), slide_path.clone());
    if let Some(url) = state.urls.get(&key) { return Ok(Some(url.clone())); }
    let url = format!("seuoffice-slide-media://archive/{}?mime={}", random_uuid()?, jscompat::encode_uri_component(mime));
    state.urls.insert(key, url.clone());
    state.sources.insert(url.clone(), Source { deck, generation, media_ref: media_ref.to_owned(), slide_path, snapshot });
    Ok(Some(url))
}

pub struct SlideMedia { pub body: Vec<u8>, pub mime: String }

fn recolor(snapshot: &Snapshot, source: &Source, bytes: &[u8]) -> Vec<u8> {
    let svg = String::from_utf8_lossy(bytes);
    let Some(path) = source.slide_path.as_deref().or(snapshot.first_slide.as_deref()).filter(|s| !s.is_empty()) else { return svg.into_owned().into_bytes() };
    let Ok(chain) = snapshot.archive.resolve_slide_chain(path) else { return svg.into_owned().into_bytes() };
    let Some(theme_xml) = chain.theme_path.as_deref().and_then(|p| snapshot.archive.read_text(p)).filter(|s| !s.is_empty()) else { return svg.into_owned().into_bytes() };
    let Ok(mut parsed) = theme::parse_theme(&theme_xml) else { return svg.into_owned().into_bytes() };
    let master = chain.master_path.as_deref().and_then(|p| snapshot.archive.read_text(p));
    let layout = chain.layout_path.as_deref().and_then(|p| snapshot.archive.read_text(p));
    let slide = snapshot.archive.read_text(path);
    parsed.clr_map.assign(theme::parse_clr_map(master.as_deref(), layout.as_deref(), slide.as_deref()));
    retint_svg_rules(&svg, &mut |name| theme::resolve_scheme_color(name, Some(&parsed), None).map(str::to_owned)).into_bytes()
}

/// Read an opaque URL only for a currently authorized view. No filesystem path
/// or unregistered URL is ever dereferenced.
pub fn read(view: ViewId, lease: &SlidesViewLease, url: &str) -> Option<SlideMedia> {
    let source = {
        let state = registry().lock().ok()?;
        let source = state.sources.get(url)?;
        let current = state.decks.get(&source.deck)?;
        if !authorized(current, view, lease) { return None; }
        source.clone()
    };
    let bytes = source.snapshot.archive.read_bytes(&source.media_ref)?;
    let mime = mime_text(display_mime(&source.media_ref, bytes));
    let (body, mime) = match mime {
        "image/svg+xml" => (recolor(&source.snapshot, &source, bytes), mime),
        "image/jpeg" => (neutralize_jpeg_orientation(bytes), mime),
        "image/tiff" => {
            let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Tiff).ok()?;
            let mut output = Cursor::new(Vec::new());
            image.write_to(&mut output, image::ImageFormat::Png).ok()?;
            (output.into_inner(), "image/png")
        }
        _ => (bytes.to_vec(), mime),
    };
    let state = registry().lock().ok()?;
    let current = state.decks.get(&source.deck)?;
    if !authorized(current, view, lease) || !state.sources.contains_key(url) { return None; }
    Some(SlideMedia { body, mime: mime.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_capability_tracks_view_history_and_release() {
        let deck = DeckId(0xfedc_0151);
        let owner = ViewId(411);
        let other = ViewId(412);
        let lease = SlidesViewLease::new();
        let other_lease = SlidesViewLease::new();
        let part = "ppt/media/image1.png";
        let old = vec![0x89, 0x50, 0x4e, 0x47, 1];
        install(deck, 1, [(owner, lease.clone())], vec![(part.into(), old.clone())], None).unwrap();
        let url = resolve(deck, 1, owner, &lease, part, None).unwrap().unwrap();
        assert_eq!(resolve(deck, 1, owner, &lease, part, None).unwrap(), Some(url.clone()));
        assert!(resolve(deck, 1, other, &other_lease, part, None).is_err());
        assert!(read(other, &other_lease, &url).is_none());
        assert_eq!(read(owner, &lease, &url).unwrap().body, old);
        install(deck, 2, [(owner, lease.clone())], vec![(part.into(), vec![0x89, 0x50, 0x4e, 0x47, 2])], None).unwrap();
        assert_eq!(read(owner, &lease, &url).unwrap().body, old);
        assert!(resolve(deck, 1, owner, &lease, part, None).is_err());
        prune(deck, &[]);
        assert!(read(owner, &lease, &url).is_none());
        let current = resolve(deck, 2, owner, &lease, part, None).unwrap().unwrap();
        lease.revoke();
        revoke_view(owner, &lease);
        assert!(read(owner, &lease, &current).is_none());
        release(deck);
    }

    #[test]
    fn tiff_is_served_as_png_without_changing_archive() {
        let deck = DeckId(0xfedc_0152);
        let view = ViewId(413);
        let lease = SlidesViewLease::new();
        let image = image::DynamicImage::new_rgba8(2, 1);
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, image::ImageFormat::Tiff).unwrap();
        install(deck, 1, [(view, lease.clone())], vec![("ppt/media/a.tiff".into(), encoded.into_inner())], None).unwrap();
        let url = resolve(deck, 1, view, &lease, "ppt/media/a.tiff", None).unwrap().unwrap();
        let result = read(view, &lease, &url).unwrap();
        assert_eq!(result.mime, "image/png");
        assert_eq!(image::guess_format(&result.body).unwrap(), image::ImageFormat::Png);
        release(deck);
    }

    #[test]
    fn embedded_font_bytes_are_scoped_to_the_live_snapshot() {
        let deck = DeckId(0xfedc_0156);
        let owner = ViewId(417);
        let lease = SlidesViewLease::new();
        let font = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let entries = vec![
            ("ppt/presentation.xml".into(), br#"<p:presentation><p:embeddedFontLst><p:embeddedFont><p:font typeface="Synthetic"/><p:bold r:id="r1"/></p:embeddedFont></p:embeddedFontLst></p:presentation>"#.to_vec()),
            ("ppt/_rels/presentation.xml.rels".into(), br#"<Relationships><Relationship Id="r1" Type="font" Target="fonts/font1.fntdata"/></Relationships>"#.to_vec()),
            ("ppt/fonts/font1.fntdata".into(), font.clone()),
        ];
        install(deck, 3, [(owner, lease.clone())], entries, None).unwrap();
        let faces = embedded_fonts(deck, 3, owner, &lease).unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].typeface, "Synthetic");
        assert_eq!(faces[0].style, pptx_engine::embedded_fonts::EmbeddedFontStyle::Bold);
        assert_eq!(faces[0].sfnt, font);
        assert!(embedded_fonts(deck, 3, ViewId(418), &lease).is_err());
        assert!(embedded_fonts(deck, 2, owner, &lease).is_err());
        release(deck);
        assert!(embedded_fonts(deck, 3, owner, &lease).is_err());
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn history_capability_moves_to_replacement_deck() {
        let old = DeckId(0xfedc_0153);
        let new = DeckId(0xfedc_0154);
        let previous_view = ViewId(414);
        let current_view = ViewId(415);
        let previous_lease = SlidesViewLease::new();
        let current_lease = SlidesViewLease::new();
        let part = "ppt/media/a.png";
        let bytes = vec![0x89, 0x50, 0x4e, 0x47, 7];
        install(old, 8, [(previous_view, previous_lease.clone())], vec![(part.into(), bytes.clone())], None).unwrap();
        let url = resolve(old, 8, previous_view, &previous_lease, part, None).unwrap().unwrap();
        assert!(install_and_transfer(old, new, 20, [(current_view, current_lease.clone())], vec![], None, &[(8, 20)]).is_err());
        assert_eq!(read(previous_view, &previous_lease, &url).unwrap().body, bytes);
        assert!(resolve(new, 20, current_view, &current_lease, part, None).is_err());
        install_and_transfer(old, new, 20, [(current_view, current_lease.clone())], vec![], None, &[(8, 21)]).unwrap();
        release(old);
        assert!(read(previous_view, &previous_lease, &url).is_none());
        assert_eq!(read(current_view, &current_lease, &url).unwrap().body, bytes);
        prune(new, &[21]);
        assert!(read(current_view, &current_lease, &url).is_some());
        prune(new, &[]);
        assert!(read(current_view, &current_lease, &url).is_none());
        release(new);
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn svg_uses_slide_theme_without_modifying_the_archive() {
        let deck = DeckId(0xfedc_0155);
        let view = ViewId(416);
        let lease = SlidesViewLease::new();
        let rel = |kind: &str, target: &str| format!(
            r#"<Relationships><Relationship Id="r1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/></Relationships>"#
        ).into_bytes();
        let svg = b"<svg><style>.MsftOfcThm_accent1_Fill{fill:#000000}</style></svg>".to_vec();
        let entries = vec![
            ("ppt/slides/slide1.xml".into(), b"<p:sld/>".to_vec()),
            ("ppt/slides/_rels/slide1.xml.rels".into(), rel("slideLayout", "../slideLayouts/slideLayout1.xml")),
            ("ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(), rel("slideMaster", "../slideMasters/slideMaster1.xml")),
            ("ppt/slideMasters/_rels/slideMaster1.xml.rels".into(), rel("theme", "../theme/theme1.xml")),
            ("ppt/theme/theme1.xml".into(), br#"<a:theme><a:themeElements><a:clrScheme><a:accent1><a:srgbClr val="123456"/></a:accent1></a:clrScheme></a:themeElements></a:theme>"#.to_vec()),
            ("ppt/media/a.svg".into(), svg),
        ];
        install(deck, 1, [(view, lease.clone())], entries, Some("ppt/slides/slide1.xml".into())).unwrap();
        let url = resolve(deck, 1, view, &lease, "ppt/media/a.svg", None).unwrap().unwrap();
        let output = read(view, &lease, &url).unwrap();
        assert_eq!(output.mime, "image/svg+xml");
        assert!(String::from_utf8(output.body).unwrap().contains("fill:#123456"));
        release(deck);
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[test]
    fn stale_view_lifetime_cannot_revoke_or_register_after_replacement() {
        let deck = DeckId(0xfedc_0157);
        let view = ViewId(419);
        let old = SlidesViewLease::new();
        let new = SlidesViewLease::new();
        let part = "ppt/media/a.png";
        let entries = || vec![(part.into(), vec![0x89, 0x50, 0x4e, 0x47, 1])];
        install(deck, 1, [(view, old.clone())], entries(), None).unwrap();
        old.revoke();
        install(deck, 2, [(view, new.clone())], entries(), None).unwrap();
        let url = resolve(deck, 2, view, &new, part, None).unwrap().unwrap();
        revoke_view(view, &old);
        assert!(read(view, &new, &url).is_some());
        assert!(read(view, &old, &url).is_none());
        assert!(install(deck, 3, [(view, old.clone())], entries(), None).is_err());
        assert!(read(view, &new, &url).is_some());
        release(deck);
    }
    #[test]
    fn deck_revoke_preserves_other_grants_and_allows_rebinding() {
        let deck = DeckId(0xfedc_0158);
        let other_deck = DeckId(0xfedc_0159);
        let view = ViewId(420);
        let other_view = ViewId(421);
        let lease = SlidesViewLease::new();
        let other_lease = SlidesViewLease::new();
        let part = "ppt/media/a.png";
        let entries = || vec![(part.into(), vec![0x89, 0x50, 0x4e, 0x47, 1])];
        install(deck, 1, [(view, lease.clone()), (other_view, other_lease.clone())], entries(), None).unwrap();
        let old_url = resolve(deck, 1, view, &lease, part, None).unwrap().unwrap();
        install(deck, 2, [(view, lease.clone()), (other_view, other_lease.clone())], entries(), None).unwrap();
        let current_url = resolve(deck, 2, view, &lease, part, None).unwrap().unwrap();
        install(other_deck, 1, [(view, lease.clone())], entries(), None).unwrap();
        let other_url = resolve(other_deck, 1, view, &lease, part, None).unwrap().unwrap();
        revoke_deck_view(deck, view, &lease).unwrap();
        assert!(read(view, &lease, &old_url).is_none());
        assert!(read(view, &lease, &current_url).is_none());
        assert!(read(other_view, &other_lease, &old_url).is_some());
        assert!(read(view, &lease, &other_url).is_some());
        install(deck, 3, [(view, lease.clone()), (other_view, other_lease.clone())], entries(), None).unwrap();
        assert!(read(view, &lease, &old_url).is_some());
        release(deck);
        release(other_deck);
    }
}
