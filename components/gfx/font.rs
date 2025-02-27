/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::ToOwned;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::{iter, str};

use app_units::Au;
use bitflags::bitflags;
use euclid::default::{Point2D, Rect, Size2D};
use log::debug;
use serde::{Deserialize, Serialize};
use servo_atoms::{atom, Atom};
use smallvec::SmallVec;
use style::computed_values::{font_stretch, font_style, font_variant_caps, font_weight};
use style::properties::style_structs::Font as FontStyleStruct;
use style::values::computed::font::{GenericFontFamily, SingleFontFamily};
use unicode_script::Script;
use webrender_api::FontInstanceKey;

use crate::font_context::{FontContext, FontSource};
use crate::font_template::FontTemplateDescriptor;
use crate::platform::font::{FontHandle, FontTable};
use crate::platform::font_context::FontContextHandle;
pub use crate::platform::font_list::fallback_font_families;
use crate::platform::font_template::FontTemplateData;
use crate::text::glyph::{ByteIndex, GlyphData, GlyphId, GlyphStore};
use crate::text::shaping::ShaperMethods;
use crate::text::Shaper;

#[macro_export]
macro_rules! ot_tag {
    ($t1:expr, $t2:expr, $t3:expr, $t4:expr) => {
        (($t1 as u32) << 24) | (($t2 as u32) << 16) | (($t3 as u32) << 8) | ($t4 as u32)
    };
}

pub const GPOS: u32 = ot_tag!('G', 'P', 'O', 'S');
pub const GSUB: u32 = ot_tag!('G', 'S', 'U', 'B');
pub const KERN: u32 = ot_tag!('k', 'e', 'r', 'n');
pub const LAST_RESORT_GLYPH_ADVANCE: FractionalPixel = 10.0;

static TEXT_SHAPING_PERFORMANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

// FontHandle encapsulates access to the platform's font API,
// e.g. quartz, FreeType. It provides access to metrics and tables
// needed by the text shaper as well as access to the underlying font
// resources needed by the graphics layer to draw glyphs.

pub trait FontHandleMethods: Sized {
    fn new_from_template(
        fctx: &FontContextHandle,
        template: Arc<FontTemplateData>,
        pt_size: Option<Au>,
    ) -> Result<Self, ()>;

    fn template(&self) -> Arc<FontTemplateData>;
    fn family_name(&self) -> Option<String>;
    fn face_name(&self) -> Option<String>;

    fn style(&self) -> font_style::T;
    fn boldness(&self) -> font_weight::T;
    fn stretchiness(&self) -> font_stretch::T;

    fn glyph_index(&self, codepoint: char) -> Option<GlyphId>;
    fn glyph_h_advance(&self, _: GlyphId) -> Option<FractionalPixel>;
    fn glyph_h_kerning(&self, glyph0: GlyphId, glyph1: GlyphId) -> FractionalPixel;

    /// Can this font do basic horizontal LTR shaping without Harfbuzz?
    fn can_do_fast_shaping(&self) -> bool;
    fn metrics(&self) -> FontMetrics;
    fn table_for_tag(&self, _: FontTableTag) -> Option<FontTable>;

    /// A unique identifier for the font, allowing comparison.
    fn identifier(&self) -> Atom;
}

// Used to abstract over the shaper's choice of fixed int representation.
pub type FractionalPixel = f64;

pub type FontTableTag = u32;

trait FontTableTagConversions {
    fn tag_to_str(&self) -> String;
}

impl FontTableTagConversions for FontTableTag {
    fn tag_to_str(&self) -> String {
        let bytes = [
            (self >> 24) as u8,
            (self >> 16) as u8,
            (self >> 8) as u8,
            (self >> 0) as u8,
        ];
        str::from_utf8(&bytes).unwrap().to_owned()
    }
}

pub trait FontTableMethods {
    fn buffer(&self) -> &[u8];
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FontMetrics {
    pub underline_size: Au,
    pub underline_offset: Au,
    pub strikeout_size: Au,
    pub strikeout_offset: Au,
    pub leading: Au,
    pub x_height: Au,
    pub em_size: Au,
    pub ascent: Au,
    pub descent: Au,
    pub max_advance: Au,
    pub average_advance: Au,
    pub line_gap: Au,
}

impl FontMetrics {
    /// Create an empty [`FontMetrics`] mainly to be used in situations where
    /// no font can be found.
    pub fn empty() -> Self {
        Self {
            underline_size: Au(0),
            underline_offset: Au(0),
            strikeout_size: Au(0),
            strikeout_offset: Au(0),
            leading: Au(0),
            x_height: Au(0),
            em_size: Au(0),
            ascent: Au(0),
            descent: Au(0),
            max_advance: Au(0),
            average_advance: Au(0),
            line_gap: Au(0),
        }
    }
}

/// `FontDescriptor` describes the parameters of a `Font`. It represents rendering a given font
/// template at a particular size, with a particular font-variant-caps applied, etc. This contrasts
/// with `FontTemplateDescriptor` in that the latter represents only the parameters inherent in the
/// font data (weight, stretch, etc.).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FontDescriptor {
    pub template_descriptor: FontTemplateDescriptor,
    pub variant: font_variant_caps::T,
    pub pt_size: Au,
}

impl<'a> From<&'a FontStyleStruct> for FontDescriptor {
    fn from(style: &'a FontStyleStruct) -> Self {
        FontDescriptor {
            template_descriptor: FontTemplateDescriptor::from(style),
            variant: style.font_variant_caps,
            pt_size: Au::from_f32_px(style.font_size.computed_size().px()),
        }
    }
}

#[derive(Debug)]
pub struct Font {
    pub handle: FontHandle,
    pub metrics: FontMetrics,
    pub descriptor: FontDescriptor,
    shaper: Option<Shaper>,
    shape_cache: RefCell<HashMap<ShapeCacheEntry, Arc<GlyphStore>>>,
    glyph_advance_cache: RefCell<HashMap<u32, FractionalPixel>>,
    pub font_key: FontInstanceKey,

    /// If this is a synthesized small caps font, then this font reference is for
    /// the version of the font used to replace lowercase ASCII letters. It's up
    /// to the consumer of this font to properly use this reference.
    pub synthesized_small_caps: Option<FontRef>,
}

impl Font {
    pub fn new(
        handle: FontHandle,
        descriptor: FontDescriptor,
        font_key: FontInstanceKey,
        synthesized_small_caps: Option<FontRef>,
    ) -> Font {
        let metrics = handle.metrics();

        Font {
            handle: handle,
            shaper: None,
            descriptor,
            metrics,
            shape_cache: RefCell::new(HashMap::new()),
            glyph_advance_cache: RefCell::new(HashMap::new()),
            font_key,
            synthesized_small_caps,
        }
    }

    /// A unique identifier for the font, allowing comparison.
    pub fn identifier(&self) -> Atom {
        self.handle.identifier()
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub struct ShapingFlags: u8 {
        /// Set if the text is entirely whitespace.
        const IS_WHITESPACE_SHAPING_FLAG = 0x01;
        /// Set if we are to ignore ligatures.
        const IGNORE_LIGATURES_SHAPING_FLAG = 0x02;
        /// Set if we are to disable kerning.
        const DISABLE_KERNING_SHAPING_FLAG = 0x04;
        /// Text direction is right-to-left.
        const RTL_FLAG = 0x08;
        /// Set if word-break is set to keep-all.
        const KEEP_ALL_FLAG = 0x10;
    }
}

/// Various options that control text shaping.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ShapingOptions {
    /// Spacing to add between each letter. Corresponds to the CSS 2.1 `letter-spacing` property.
    /// NB: You will probably want to set the `IGNORE_LIGATURES_SHAPING_FLAG` if this is non-null.
    pub letter_spacing: Option<Au>,
    /// Spacing to add between each word. Corresponds to the CSS 2.1 `word-spacing` property.
    pub word_spacing: Au,
    /// The Unicode script property of the characters in this run.
    pub script: Script,
    /// Various flags.
    pub flags: ShapingFlags,
}

/// An entry in the shape cache.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ShapeCacheEntry {
    text: String,
    options: ShapingOptions,
}

impl Font {
    pub fn shape_text(&mut self, text: &str, options: &ShapingOptions) -> Arc<GlyphStore> {
        let this = self as *const Font;
        let mut shaper = self.shaper.take();

        let lookup_key = ShapeCacheEntry {
            text: text.to_owned(),
            options: *options,
        };
        let result = self
            .shape_cache
            .borrow_mut()
            .entry(lookup_key)
            .or_insert_with(|| {
                let start_time = Instant::now();
                let mut glyphs = GlyphStore::new(
                    text.len(),
                    options
                        .flags
                        .contains(ShapingFlags::IS_WHITESPACE_SHAPING_FLAG),
                    options.flags.contains(ShapingFlags::RTL_FLAG),
                );

                if self.can_do_fast_shaping(text, options) {
                    debug!("shape_text: Using ASCII fast path.");
                    self.shape_text_fast(text, options, &mut glyphs);
                } else {
                    debug!("shape_text: Using Harfbuzz.");
                    if shaper.is_none() {
                        shaper = Some(Shaper::new(this));
                    }
                    shaper
                        .as_ref()
                        .unwrap()
                        .shape_text(text, options, &mut glyphs);
                }

                let end_time = Instant::now();
                TEXT_SHAPING_PERFORMANCE_COUNTER.fetch_add(
                    (end_time.duration_since(start_time).as_nanos()) as usize,
                    Ordering::Relaxed,
                );
                Arc::new(glyphs)
            })
            .clone();
        self.shaper = shaper;
        result
    }

    fn can_do_fast_shaping(&self, text: &str, options: &ShapingOptions) -> bool {
        options.script == Script::Latin &&
            !options.flags.contains(ShapingFlags::RTL_FLAG) &&
            self.handle.can_do_fast_shaping() &&
            text.is_ascii()
    }

    /// Fast path for ASCII text that only needs simple horizontal LTR kerning.
    fn shape_text_fast(&self, text: &str, options: &ShapingOptions, glyphs: &mut GlyphStore) {
        let mut prev_glyph_id = None;
        for (i, byte) in text.bytes().enumerate() {
            let character = byte as char;
            let glyph_id = match self.glyph_index(character) {
                Some(id) => id,
                None => continue,
            };

            let mut advance = Au::from_f64_px(self.glyph_h_advance(glyph_id));
            if character == ' ' {
                // https://drafts.csswg.org/css-text-3/#word-spacing-property
                advance += options.word_spacing;
            }
            if let Some(letter_spacing) = options.letter_spacing {
                advance += letter_spacing;
            }
            let offset = prev_glyph_id.map(|prev| {
                let h_kerning = Au::from_f64_px(self.glyph_h_kerning(prev, glyph_id));
                advance += h_kerning;
                Point2D::new(h_kerning, Au(0))
            });

            let glyph = GlyphData::new(glyph_id, advance, offset, true, true);
            glyphs.add_glyph_for_byte_index(ByteIndex(i as isize), character, &glyph);
            prev_glyph_id = Some(glyph_id);
        }
        glyphs.finalize_changes();
    }

    pub fn table_for_tag(&self, tag: FontTableTag) -> Option<FontTable> {
        let result = self.handle.table_for_tag(tag);
        let status = if result.is_some() {
            "Found"
        } else {
            "Didn't find"
        };

        debug!(
            "{} font table[{}] with family={}, face={}",
            status,
            tag.tag_to_str(),
            self.handle
                .family_name()
                .unwrap_or("unavailable".to_owned()),
            self.handle.face_name().unwrap_or("unavailable".to_owned())
        );

        result
    }

    #[inline]
    pub fn glyph_index(&self, codepoint: char) -> Option<GlyphId> {
        let codepoint = match self.descriptor.variant {
            font_variant_caps::T::SmallCaps => codepoint.to_ascii_uppercase(),
            font_variant_caps::T::Normal => codepoint,
        };
        self.handle.glyph_index(codepoint)
    }

    pub fn has_glyph_for(&self, codepoint: char) -> bool {
        self.glyph_index(codepoint).is_some()
    }

    pub fn glyph_h_kerning(&self, first_glyph: GlyphId, second_glyph: GlyphId) -> FractionalPixel {
        self.handle.glyph_h_kerning(first_glyph, second_glyph)
    }

    pub fn glyph_h_advance(&self, glyph: GlyphId) -> FractionalPixel {
        *self
            .glyph_advance_cache
            .borrow_mut()
            .entry(glyph)
            .or_insert_with(|| {
                match self.handle.glyph_h_advance(glyph) {
                    Some(adv) => adv,
                    None => LAST_RESORT_GLYPH_ADVANCE as FractionalPixel, // FIXME: Need fallback strategy
                }
            })
    }
}

pub type FontRef = Rc<RefCell<Font>>;

/// A `FontGroup` is a prioritised list of fonts for a given set of font styles. It is used by
/// `TextRun` to decide which font to render a character with. If none of the fonts listed in the
/// styles are suitable, a fallback font may be used.
#[derive(Debug)]
pub struct FontGroup {
    descriptor: FontDescriptor,
    families: SmallVec<[FontGroupFamily; 8]>,
    last_matching_fallback: Option<FontRef>,
}

impl FontGroup {
    pub fn new(style: &FontStyleStruct) -> FontGroup {
        let descriptor = FontDescriptor::from(style);

        let families = style
            .font_family
            .families
            .iter()
            .map(|family| FontGroupFamily::new(descriptor.clone(), &family))
            .collect();

        FontGroup {
            descriptor,
            families,
            last_matching_fallback: None,
        }
    }

    /// Finds the first font, or else the first fallback font, which contains a glyph for
    /// `codepoint`. If no such font is found, returns the first available font or fallback font
    /// (which will cause a "glyph not found" character to be rendered). If no font at all can be
    /// found, returns None.
    pub fn find_by_codepoint<S: FontSource>(
        &mut self,
        mut font_context: &mut FontContext<S>,
        codepoint: char,
    ) -> Option<FontRef> {
        let should_look_for_small_caps = self.descriptor.variant == font_variant_caps::T::SmallCaps &&
            codepoint.is_ascii_lowercase();
        let font_or_synthesized_small_caps = |font: FontRef| {
            if should_look_for_small_caps {
                let font = font.borrow();
                if font.synthesized_small_caps.is_some() {
                    return font.synthesized_small_caps.clone();
                }
            }
            Some(font)
        };

        let has_glyph = |font: &FontRef| font.borrow().has_glyph_for(codepoint);

        if let Some(font) = self.find(&mut font_context, |font| has_glyph(font)) {
            return font_or_synthesized_small_caps(font);
        }

        if let Some(ref last_matching_fallback) = self.last_matching_fallback {
            if has_glyph(&last_matching_fallback) {
                return font_or_synthesized_small_caps(last_matching_fallback.clone());
            }
        }

        if let Some(font) = self.find_fallback(&mut font_context, Some(codepoint), has_glyph) {
            self.last_matching_fallback = Some(font.clone());
            return font_or_synthesized_small_caps(font);
        }

        self.first(&mut font_context)
    }

    /// Find the first available font in the group, or the first available fallback font.
    pub fn first<S: FontSource>(
        &mut self,
        mut font_context: &mut FontContext<S>,
    ) -> Option<FontRef> {
        self.find(&mut font_context, |_| true)
            .or_else(|| self.find_fallback(&mut font_context, None, |_| true))
    }

    /// Find a font which returns true for `predicate`. This method mutates because we may need to
    /// load new font data in the process of finding a suitable font.
    fn find<S, P>(&mut self, mut font_context: &mut FontContext<S>, predicate: P) -> Option<FontRef>
    where
        S: FontSource,
        P: FnMut(&FontRef) -> bool,
    {
        self.families
            .iter_mut()
            .filter_map(|family| family.font(&mut font_context))
            .find(predicate)
    }

    /// Attempts to find a suitable fallback font which matches the `predicate`. The default
    /// family (i.e. "serif") will be tried first, followed by platform-specific family names.
    /// If a `codepoint` is provided, then its Unicode block may be used to refine the list of
    /// family names which will be tried.
    fn find_fallback<S, P>(
        &mut self,
        font_context: &mut FontContext<S>,
        codepoint: Option<char>,
        predicate: P,
    ) -> Option<FontRef>
    where
        S: FontSource,
        P: FnMut(&FontRef) -> bool,
    {
        iter::once(FontFamilyDescriptor::default())
            .chain(fallback_font_families(codepoint).into_iter().map(|family| {
                FontFamilyDescriptor::new(FontFamilyName::from(family), FontSearchScope::Local)
            }))
            .filter_map(|family| font_context.font(&self.descriptor, &family))
            .find(predicate)
    }
}

/// A `FontGroupFamily` is a single font family in a `FontGroup`. It corresponds to one of the
/// families listed in the `font-family` CSS property. The corresponding font data is lazy-loaded,
/// only if actually needed.
#[derive(Debug)]
struct FontGroupFamily {
    font_descriptor: FontDescriptor,
    family_descriptor: FontFamilyDescriptor,
    loaded: bool,
    font: Option<FontRef>,
}

impl FontGroupFamily {
    fn new(font_descriptor: FontDescriptor, family: &SingleFontFamily) -> FontGroupFamily {
        let family_descriptor =
            FontFamilyDescriptor::new(FontFamilyName::from(family), FontSearchScope::Any);

        FontGroupFamily {
            font_descriptor,
            family_descriptor,
            loaded: false,
            font: None,
        }
    }

    /// Returns the font within this family which matches the style. We'll fetch the data from the
    /// `FontContext` the first time this method is called, and return a cached reference on
    /// subsequent calls.
    fn font<S: FontSource>(&mut self, font_context: &mut FontContext<S>) -> Option<FontRef> {
        if !self.loaded {
            self.font = font_context.font(&self.font_descriptor, &self.family_descriptor);
            self.loaded = true;
        }

        self.font.clone()
    }
}

pub struct RunMetrics {
    // may be negative due to negative width (i.e., kerning of '.' in 'P.T.')
    pub advance_width: Au,
    pub ascent: Au,  // nonzero
    pub descent: Au, // nonzero
    // this bounding box is relative to the left origin baseline.
    // so, bounding_box.position.y = -ascent
    pub bounding_box: Rect<Au>,
}

impl RunMetrics {
    pub fn new(advance: Au, ascent: Au, descent: Au) -> RunMetrics {
        let bounds = Rect::new(
            Point2D::new(Au(0), -ascent),
            Size2D::new(advance, ascent + descent),
        );

        // TODO(Issue #125): support loose and tight bounding boxes; using the
        // ascent+descent and advance is sometimes too generous and
        // looking at actual glyph extents can yield a tighter box.

        RunMetrics {
            advance_width: advance,
            bounding_box: bounds,
            ascent,
            descent,
        }
    }
}

pub fn get_and_reset_text_shaping_performance_counter() -> usize {
    let value = TEXT_SHAPING_PERFORMANCE_COUNTER.swap(0, Ordering::SeqCst);
    value
}

/// The scope within which we will look for a font.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum FontSearchScope {
    /// All fonts will be searched, including those specified via `@font-face` rules.
    Any,

    /// Only local system fonts will be searched.
    Local,
}

/// A font family name used in font selection.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum FontFamilyName {
    /// A specific name such as `"Arial"`
    Specific(Atom),

    /// A generic name such as `sans-serif`
    Generic(Atom),
}

impl FontFamilyName {
    pub fn name(&self) -> &str {
        match *self {
            FontFamilyName::Specific(ref name) => name,
            FontFamilyName::Generic(ref name) => name,
        }
    }
}

impl<'a> From<&'a SingleFontFamily> for FontFamilyName {
    fn from(other: &'a SingleFontFamily) -> FontFamilyName {
        match *other {
            SingleFontFamily::FamilyName(ref family_name) => {
                FontFamilyName::Specific(family_name.name.clone())
            },

            SingleFontFamily::Generic(generic) => FontFamilyName::Generic(match generic {
                GenericFontFamily::None => panic!("Shouldn't appear in style"),
                GenericFontFamily::Serif => atom!("serif"),
                GenericFontFamily::SansSerif => atom!("sans-serif"),
                GenericFontFamily::Monospace => atom!("monospace"),
                GenericFontFamily::Cursive => atom!("cursive"),
                GenericFontFamily::Fantasy => atom!("fantasy"),
                GenericFontFamily::SystemUi => atom!("system-ui"),
            }),
        }
    }
}

impl<'a> From<&'a str> for FontFamilyName {
    fn from(other: &'a str) -> FontFamilyName {
        FontFamilyName::Specific(Atom::from(other))
    }
}

/// The font family parameters for font selection.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FontFamilyDescriptor {
    pub name: FontFamilyName,
    pub scope: FontSearchScope,
}

impl FontFamilyDescriptor {
    pub fn new(name: FontFamilyName, scope: FontSearchScope) -> FontFamilyDescriptor {
        FontFamilyDescriptor { name, scope }
    }

    fn default() -> FontFamilyDescriptor {
        FontFamilyDescriptor {
            name: FontFamilyName::Generic(atom!("serif")),
            scope: FontSearchScope::Local,
        }
    }

    pub fn name(&self) -> &str {
        self.name.name()
    }
}
