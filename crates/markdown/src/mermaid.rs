use collections::HashMap;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, ImageSource, RenderImage, StyledText, Task, img,
    pulsating_between,
};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use ui::prelude::*;

use crate::parser::{CodeBlockKind, MarkdownEvent, MarkdownTag};

use super::{Markdown, MarkdownStyle, ParsedMarkdown};

type MermaidDiagramCache = HashMap<MermaidCacheKey, Arc<CachedMermaidDiagram>>;

#[derive(Clone, Debug)]
pub(crate) struct ParsedMarkdownMermaidDiagram {
    pub(crate) content_range: Range<usize>,
    pub(crate) contents: ParsedMarkdownMermaidDiagramContents,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParsedMarkdownMermaidDiagramContents {
    pub(crate) contents: SharedString,
    pub(crate) scale: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MermaidCacheKey {
    pub(crate) contents: ParsedMarkdownMermaidDiagramContents,
    pub(crate) is_light: bool,
}

#[derive(Default, Clone)]
pub(crate) struct MermaidState {
    cache: MermaidDiagramCache,
    order: Vec<MermaidCacheKey>,
    // Mirrors the appearance used by the most recent `update()` call. The
    // default value is meaningless on its own — it is unused until `update()`
    // has stamped it, which happens before any mermaid block can be rendered
    // (parse → update → render).
    pub(crate) is_light: bool,
}

struct CachedMermaidDiagram {
    render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
    fallback_image: Option<Arc<RenderImage>>,
    _task: Task<()>,
}

impl MermaidState {
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    fn get_fallback_image(
        idx: usize,
        old_order: &[MermaidCacheKey],
        new_order_len: usize,
        cache: &MermaidDiagramCache,
    ) -> Option<Arc<RenderImage>> {
        if old_order.len() != new_order_len {
            return None;
        }

        old_order.get(idx).and_then(|old_key| {
            cache.get(old_key).and_then(|old_cached| {
                old_cached
                    .render_image
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    .or_else(|| old_cached.fallback_image.clone())
            })
        })
    }

    pub(crate) fn update(&mut self, parsed: &ParsedMarkdown, cx: &mut Context<Markdown>) {
        let is_light = cx.theme().appearance().is_light();
        self.is_light = is_light;

        let mut new_order = Vec::new();
        for mermaid_diagram in parsed.mermaid_diagrams.values() {
            new_order.push(MermaidCacheKey {
                contents: mermaid_diagram.contents.clone(),
                is_light,
            });
        }

        for (idx, new_key) in new_order.iter().enumerate() {
            if !self.cache.contains_key(new_key) {
                let fallback =
                    Self::get_fallback_image(idx, &self.order, new_order.len(), &self.cache);
                self.cache.insert(
                    new_key.clone(),
                    Arc::new(CachedMermaidDiagram::new(new_key.clone(), fallback, cx)),
                );
            }
        }

        let new_order_set: std::collections::HashSet<_> = new_order.iter().cloned().collect();
        self.cache
            .retain(|key, _| new_order_set.contains(key));
        self.order = new_order;
    }
}

impl CachedMermaidDiagram {
    fn new(
        key: MermaidCacheKey,
        fallback_image: Option<Arc<RenderImage>>,
        cx: &mut Context<Markdown>,
    ) -> Self {
        let render_image = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
        let render_image_clone = render_image.clone();
        let svg_renderer = cx.svg_renderer();

        let task = cx.spawn(async move |this, cx| {
            let value = cx
                .background_spawn(async move {
                    let theme = if key.is_light {
                        mermaid_rs_renderer::Theme::modern()
                    } else {
                        mermaid_dark_theme()
                    };
                    let options = mermaid_rs_renderer::RenderOptions {
                        theme,
                        layout: mermaid_rs_renderer::LayoutConfig::default(),
                    };
                    let svg_string = mermaid_rs_renderer::render_with_options(
                        &key.contents.contents,
                        options,
                    )?;
                    let scale = key.contents.scale as f32 / 100.0;
                    svg_renderer
                        .render_single_frame(svg_string.as_bytes(), scale)
                        .map_err(|error| anyhow::anyhow!("{error}"))
                })
                .await;
            let _ = render_image_clone.set(value);
            this.update(cx, |_, cx| {
                cx.notify();
            })
            .ok();
        });

        Self {
            render_image,
            fallback_image,
            _task: task,
        }
    }

    #[cfg(test)]
    fn new_for_test(
        render_image: Option<Arc<RenderImage>>,
        fallback_image: Option<Arc<RenderImage>>,
    ) -> Self {
        let result = Arc::new(OnceLock::new());
        if let Some(render_image) = render_image {
            let _ = result.set(Ok(render_image));
        }
        Self {
            render_image: result,
            fallback_image,
            _task: Task::ready(()),
        }
    }
}

// Derived from Zed's "One Dark" palette. Keeps the same field structure as
// `Theme::modern()` and only overrides color-valued fields.
fn mermaid_dark_theme() -> mermaid_rs_renderer::Theme {
    let hex = |value: &str| value.to_string();
    let mut theme = mermaid_rs_renderer::Theme::modern();
    theme.background = hex("#2f343e");
    theme.primary_color = hex("#3b414d");
    theme.secondary_color = hex("#464b57");
    theme.tertiary_color = hex("#2e343e");
    theme.primary_text_color = hex("#dce0e5");
    theme.text_color = hex("#dce0e5");
    theme.primary_border_color = hex("#7d8494");
    theme.line_color = hex("#9ca3af");
    theme.edge_label_background = hex("#2f343e");
    theme.cluster_background = hex("#353b47");
    theme.cluster_border = hex("#545a67");
    theme.sequence_actor_fill = hex("#3b414d");
    theme.sequence_actor_border = hex("#7d8494");
    theme.sequence_actor_line = hex("#6b7280");
    theme.sequence_note_fill = hex("#4b3f2c");
    theme.sequence_note_border = hex("#dec184");
    theme.sequence_activation_fill = hex("#3b414d");
    theme.sequence_activation_border = hex("#7d8494");
    theme.pie_title_text_color = hex("#dce0e5");
    theme.pie_section_text_color = hex("#dce0e5");
    theme.pie_legend_text_color = hex("#dce0e5");
    theme.pie_stroke_color = hex("#94a3b8");
    theme.pie_outer_stroke_color = hex("#464b57");
    theme
}

fn parse_mermaid_info(info: &str) -> Option<u32> {
    let mut parts = info.split_whitespace();
    if parts.next()? != "mermaid" {
        return None;
    }

    Some(
        parts
            .next()
            .and_then(|scale| scale.parse().ok())
            .unwrap_or(100)
            .clamp(10, 500),
    )
}

pub(crate) fn extract_mermaid_diagrams(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMarkdownMermaidDiagram> {
    let mut mermaid_diagrams = BTreeMap::default();

    for (source_range, event) in events {
        let MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, metadata }) = event else {
            continue;
        };
        let CodeBlockKind::FencedLang(info) = kind else {
            continue;
        };
        let Some(scale) = parse_mermaid_info(info.as_ref()) else {
            continue;
        };

        let contents = source[metadata.content_range.clone()]
            .strip_suffix('\n')
            .unwrap_or(&source[metadata.content_range.clone()])
            .to_string();
        mermaid_diagrams.insert(
            source_range.start,
            ParsedMarkdownMermaidDiagram {
                content_range: metadata.content_range.clone(),
                contents: ParsedMarkdownMermaidDiagramContents {
                    contents: contents.into(),
                    scale,
                },
            },
        );
    }

    mermaid_diagrams
}

pub(crate) fn render_mermaid_diagram(
    parsed: &ParsedMarkdownMermaidDiagram,
    mermaid_state: &MermaidState,
    style: &MarkdownStyle,
) -> AnyElement {
    let lookup_key = MermaidCacheKey {
        contents: parsed.contents.clone(),
        is_light: mermaid_state.is_light,
    };
    let cached = mermaid_state.cache.get(&lookup_key);
    let mut container = div().w_full();
    container.style().refine(&style.code_block);

    if let Some(result) = cached.and_then(|cached| cached.render_image.get()) {
        match result {
            Ok(render_image) => container
                .child(
                    div().w_full().child(
                        img(ImageSource::Render(render_image.clone()))
                            .max_w_full()
                            .with_fallback(|| {
                                div()
                                    .child(Label::new("Failed to load mermaid diagram"))
                                    .into_any_element()
                            }),
                    ),
                )
                .into_any_element(),
            Err(_) => container
                .child(StyledText::new(parsed.contents.contents.clone()))
                .into_any_element(),
        }
    } else if let Some(fallback) = cached.and_then(|cached| cached.fallback_image.as_ref()) {
        container
            .child(
                div()
                    .w_full()
                    .child(
                        img(ImageSource::Render(fallback.clone()))
                            .max_w_full()
                            .with_fallback(|| {
                                div()
                                    .child(Label::new("Failed to load mermaid diagram"))
                                    .into_any_element()
                            }),
                    )
                    .with_animation(
                        "mermaid-fallback-pulse",
                        Animation::new(Duration::from_secs(2))
                            .repeat()
                            .with_easing(pulsating_between(0.6, 1.0)),
                        |element, delta| element.opacity(delta),
                    ),
            )
            .into_any_element()
    } else {
        container
            .child(
                Label::new("Rendering mermaid diagram...")
                    .color(Color::Muted)
                    .with_animation(
                        "mermaid-loading-pulse",
                        Animation::new(Duration::from_secs(2))
                            .repeat()
                            .with_easing(pulsating_between(0.4, 0.8)),
                        |label, delta| label.alpha(delta),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CachedMermaidDiagram, MermaidCacheKey, MermaidDiagramCache, MermaidState,
        ParsedMarkdownMermaidDiagramContents, extract_mermaid_diagrams, parse_mermaid_info,
    };
    use crate::{
        CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
        MarkdownStyle,
    };
    use collections::HashMap;
    use gpui::{Context, IntoElement, Render, RenderImage, TestAppContext, Window, size};
    use std::sync::Arc;
    use ui::prelude::*;

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    fn render_markdown_with_options(
        markdown: &str,
        options: MarkdownOptions,
        cx: &mut TestAppContext,
    ) -> crate::RenderedText {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(markdown.to_string().into(), None, None, options, cx)
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default()).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        border: false,
                    },
                )
            },
        );
        rendered.text
    }

    fn mock_render_image(cx: &mut TestAppContext) -> Arc<RenderImage> {
        cx.update(|cx| {
            cx.svg_renderer()
                .render_single_frame(
                    br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
                    1.0,
                )
                .unwrap()
        })
    }

    fn mermaid_contents(contents: &str) -> ParsedMarkdownMermaidDiagramContents {
        ParsedMarkdownMermaidDiagramContents {
            contents: contents.to_string().into(),
            scale: 100,
        }
    }

    fn light_key(contents: &str) -> MermaidCacheKey {
        MermaidCacheKey {
            contents: mermaid_contents(contents),
            is_light: true,
        }
    }

    fn mermaid_sequence(diagrams: &[&str]) -> Vec<MermaidCacheKey> {
        diagrams.iter().map(|diagram| light_key(diagram)).collect()
    }

    fn mermaid_fallback(
        new_diagram: &str,
        new_full_order: &[MermaidCacheKey],
        old_full_order: &[MermaidCacheKey],
        cache: &MermaidDiagramCache,
    ) -> Option<Arc<RenderImage>> {
        let new_key = light_key(new_diagram);
        let idx = new_full_order.iter().position(|key| key == &new_key)?;
        MermaidState::get_fallback_image(idx, old_full_order, new_full_order.len(), cache)
    }

    #[test]
    fn test_parse_mermaid_info() {
        assert_eq!(parse_mermaid_info("mermaid"), Some(100));
        assert_eq!(parse_mermaid_info("mermaid 150"), Some(150));
        assert_eq!(parse_mermaid_info("mermaid 5"), Some(10));
        assert_eq!(parse_mermaid_info("mermaid 999"), Some(500));
        assert_eq!(parse_mermaid_info("rust"), None);
    }

    #[test]
    fn test_extract_mermaid_diagrams_parses_scale() {
        let markdown = "```mermaid 150\ngraph TD;\n```\n\n```rust\nfn main() {}\n```";
        let events = crate::parser::parse_markdown_with_options(markdown, false, false).events;
        let diagrams = extract_mermaid_diagrams(markdown, &events);

        assert_eq!(diagrams.len(), 1);
        let diagram = diagrams.values().next().unwrap();
        assert_eq!(diagram.contents.contents, "graph TD;");
        assert_eq!(diagram.contents.scale, 150);
    }

    #[gpui::test]
    fn test_mermaid_fallback_on_edit(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph B", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);

        let svg_b = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            light_key("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );
        cache.insert(
            light_key("graph B"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(svg_b.clone()),
                None,
            )),
        );
        cache.insert(
            light_key("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );

        let fallback =
            mermaid_fallback("graph B modified", &new_full_order, &old_full_order, &cache);

        assert_eq!(fallback.as_ref().map(|image| image.id), Some(svg_b.id));
    }

    #[gpui::test]
    fn test_mermaid_no_fallback_on_add_in_middle(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph NEW", "graph C"]);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            light_key("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );
        cache.insert(
            light_key("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );

        let fallback = mermaid_fallback("graph NEW", &new_full_order, &old_full_order, &cache);

        assert!(fallback.is_none());
    }

    #[gpui::test]
    fn test_mermaid_fallback_chains_on_rapid_edits(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified again", "graph C"]);

        let original_svg = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            light_key("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );
        cache.insert(
            light_key("graph B modified"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                None,
                Some(original_svg.clone()),
            )),
        );
        cache.insert(
            light_key("graph C"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );

        let fallback = mermaid_fallback(
            "graph B modified again",
            &new_full_order,
            &old_full_order,
            &cache,
        );

        assert_eq!(
            fallback.as_ref().map(|image| image.id),
            Some(original_svg.id)
        );
    }

    #[gpui::test]
    fn test_mermaid_fallback_with_duplicate_blocks_edit_second(cx: &mut TestAppContext) {
        let old_full_order = mermaid_sequence(&["graph A", "graph A", "graph B"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph A edited", "graph B"]);

        let svg_a = mock_render_image(cx);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            light_key("graph A"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(svg_a.clone()),
                None,
            )),
        );
        cache.insert(
            light_key("graph B"),
            Arc::new(CachedMermaidDiagram::new_for_test(
                Some(mock_render_image(cx)),
                None,
            )),
        );

        let fallback = mermaid_fallback("graph A edited", &new_full_order, &old_full_order, &cache);

        assert_eq!(fallback.as_ref().map(|image| image.id), Some(svg_a.id));
    }

    fn set_theme_appearance(cx: &mut TestAppContext, appearance: theme::Appearance) {
        cx.update(|cx| {
            let current = cx.theme().clone();
            if current.appearance == appearance {
                return;
            }
            let mut next = (*current).clone();
            next.appearance = appearance;
            theme::GlobalTheme::update_theme(cx, Arc::new(next));
        });
    }

    // Exercises the full observe-global → update() path: populates the cache
    // under a light appearance, flips the global theme to dark, and asserts
    // that update() stamped new dark-appearance keys and evicted the light
    // ones. Covers the behavior that the Markdown entity's GlobalTheme
    // subscription drives in production.
    #[gpui::test]
    fn test_mermaid_update_rekeys_on_appearance_change(cx: &mut TestAppContext) {
        struct TestWindow;
        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);
        set_theme_appearance(cx, theme::Appearance::Light);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                "```mermaid\ngraph TD;\n```".into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();

        markdown.read_with(cx, |markdown, _| {
            assert!(markdown.mermaid_state.is_light);
            assert_eq!(markdown.mermaid_state.order.len(), 1);
            assert!(
                markdown.mermaid_state.order.iter().all(|key| key.is_light),
                "cache should be populated with light-appearance keys",
            );
        });

        set_theme_appearance(cx, theme::Appearance::Dark);
        cx.run_until_parked();

        markdown.read_with(cx, |markdown, _| {
            assert!(!markdown.mermaid_state.is_light);
            assert_eq!(markdown.mermaid_state.order.len(), 1);
            assert!(
                markdown.mermaid_state.order.iter().all(|key| !key.is_light),
                "observer should have re-keyed the cache to dark-appearance",
            );
            for key in &markdown.mermaid_state.order {
                assert!(
                    markdown.mermaid_state.cache.contains_key(key),
                    "dark-appearance keys must be present in the cache",
                );
                let light_twin = MermaidCacheKey {
                    contents: key.contents.clone(),
                    is_light: true,
                };
                assert!(
                    !markdown.mermaid_state.cache.contains_key(&light_twin),
                    "stale light-appearance entries must have been evicted",
                );
            }
        });
    }

    #[gpui::test]
    fn test_mermaid_rendering_replaces_code_block_text(cx: &mut TestAppContext) {
        let rendered = render_markdown_with_options(
            "```mermaid\ngraph TD;\n```",
            MarkdownOptions {
                render_mermaid_diagrams: true,
                ..Default::default()
            },
            cx,
        );

        let text = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!text.contains("graph TD;"));
    }

    #[gpui::test]
    fn test_mermaid_source_anchor_maps_inside_block(cx: &mut TestAppContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                "```mermaid\ngraph TD;\n```".into(),
                None,
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        let render_image = mock_render_image(cx);
        markdown.update(cx, |markdown, _| {
            let contents = markdown
                .parsed_markdown
                .mermaid_diagrams
                .values()
                .next()
                .unwrap()
                .contents
                .clone();
            let key = MermaidCacheKey {
                contents,
                is_light: markdown.mermaid_state.is_light,
            };
            markdown.mermaid_state.cache.insert(
                key.clone(),
                Arc::new(CachedMermaidDiagram::new_for_test(Some(render_image), None)),
            );
            markdown.mermaid_state.order = vec![key];
        });

        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown.clone(), MarkdownStyle::default())
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        border: false,
                    })
            },
        );

        let mermaid_diagram = markdown.update(cx, |markdown, _| {
            markdown
                .parsed_markdown
                .mermaid_diagrams
                .values()
                .next()
                .unwrap()
                .clone()
        });
        assert!(
            rendered
                .text
                .position_for_source_index(mermaid_diagram.content_range.start)
                .is_some()
        );
        assert!(
            rendered
                .text
                .position_for_source_index(mermaid_diagram.content_range.end.saturating_sub(1))
                .is_some()
        );
    }
}
