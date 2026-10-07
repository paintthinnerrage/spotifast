//! Alphabet hints, in the style of Vimium C.
//!
//! `N` (when a text field does not have the keyboard) names every visible
//! clickable control with a short label drawn from the home row. The labels
//! are capitals; the keys are lowercase. Typing narrows the set, and the
//! last remaining control is pressed. Backspace deletes a letter, Space cycles
//! labels that sit on top of each other, and Esc closes the overlay.
//!
//! Vimium walks the DOM. This overlay walks the accessibility tree egui
//! already builds for screen readers: a control that can be clicked has a
//! node, a rectangle, and a Click action. The press is that same Click
//! action, delivered on the next frame, so the widget's own `clicked()` path
//! runs. No control has to know the overlay exists.

use std::collections::HashMap;

use egui::accesskit::{Action, ActionRequest, Node, NodeId, Rect as NodeRect, Role, TreeId};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, Context, CornerRadius, Event, FontFamily, FontId, Id, Key, LayerId, Modifiers, Order,
    Painter, Plugin, Pos2, RawInput, Rect, Stroke, StrokeKind, Vec2, ViewportId,
};

/// Vimium's default hint alphabet: home-row keys first, so the shortest
/// labels land on the easiest keys.
const HINT_CHARS: &str = "sadfjklewcmpgh";

const INK: Color32 = Color32::from_rgb(0x30, 0x25, 0x05);
const MATCHED: Color32 = Color32::from_rgb(0x82, 0x77, 0x48);
const FILL: Color32 = Color32::from_rgb(0xFF, 0xC5, 0x2A);
const EDGE: Color32 = Color32::from_rgb(0xC3, 0x8A, 0x16);

#[derive(Clone)]
struct Target {
    node: NodeId,
    /// Visible part of the control, in points.
    rect: Rect,
    hint: String,
}

/// One plugin for the whole app. The keyboard is global; the rectangles
/// belong to the viewport that drew them.
#[derive(Default)]
struct LinkHints {
    active: bool,
    typed: String,
    /// Rotates which overlapping label is painted last.
    stack: usize,
    pending: Option<(ViewportId, NodeId)>,
    /// AccessKit was off until hints turned it on, so leaving can turn it
    /// off again and a screen reader that was already attached stays on.
    disable_when_done: bool,
    tree_seen: bool,
    viewports: HashMap<ViewportId, Vec<Target>>,
}

pub(crate) fn install(ctx: &Context) {
    ctx.add_plugin(LinkHints::default());
}

impl Plugin for LinkHints {
    fn debug_name(&self) -> &'static str {
        "link hints"
    }

    fn input_hook(&mut self, ctx: &Context, input: &mut RawInput) {
        let viewport = ctx.viewport_id();
        if self.pending.is_some_and(|(id, _)| id == viewport)
            && let Some((_, node)) = self.pending.take()
        {
            input
                .events
                .push(Event::AccessKitActionRequest(ActionRequest {
                    target_tree: TreeId::ROOT,
                    target_node: node,
                    action: Action::Click,
                    data: None,
                }));
        }

        let events = std::mem::take(&mut input.events);
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            if !self.active {
                let opens = matches!(
                    event,
                    Event::Key {
                        key: Key::N,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if modifiers == Modifiers::NONE
                ) && !ctx.text_edit_focused();
                if opens {
                    self.enter(ctx);
                    continue;
                }
                kept.push(event);
                continue;
            }
            match event {
                Event::Text(_) => {}
                Event::Key {
                    pressed: true,
                    key,
                    modifiers,
                    repeat,
                    ..
                } => self.on_key(ctx, viewport, key, modifiers, repeat),
                other => kept.push(other),
            }
        }
        input.events = kept;
    }

    fn on_end_pass(&mut self, ui: &mut egui::Ui) {
        if self.active {
            self.paint(ui.ctx());
        }
    }

    fn output_hook(&mut self, ctx: &Context, output: &mut egui::FullOutput) {
        let Some(update) = output.platform_output.accesskit_update.as_ref() else {
            return;
        };
        self.tree_seen = true;
        if !self.active {
            return;
        }
        let viewport = ctx.viewport_id();
        let screen = ctx.viewport_rect();
        if !screen.is_positive() {
            return;
        }
        let mut next = controls(update.nodes.iter().map(|(id, node)| (*id, node)), screen);
        self.keep_or_assign(&mut next, viewport);
        self.viewports.insert(viewport, next);
    }
}

impl LinkHints {
    fn enter(&mut self, ctx: &Context) {
        self.disable_when_done = !self.tree_seen;
        self.active = true;
        self.typed.clear();
        self.stack = 0;
        // The tree is built at the start of the pass, and this hook runs
        // before that, so the labels exist on the frame after `N`.
        ctx.enable_accesskit();
        ctx.request_repaint();
    }

    fn leave(&mut self, ctx: &Context) {
        self.active = false;
        self.typed.clear();
        self.stack = 0;
        if self.disable_when_done {
            ctx.disable_accesskit();
            self.disable_when_done = false;
            self.tree_seen = false;
        }
    }

    fn on_key(
        &mut self,
        ctx: &Context,
        viewport: ViewportId,
        key: Key,
        modifiers: Modifiers,
        repeat: bool,
    ) {
        if modifiers.command || modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
            return;
        }
        match key {
            Key::Escape => self.leave(ctx),
            Key::Backspace => {
                self.typed.pop();
            }
            Key::Space => self.stack = self.stack.wrapping_add(1),
            key => {
                if repeat {
                    return;
                }
                let Some(ch) = hint_letter(key) else {
                    return;
                };
                if !HINT_CHARS.contains(ch) {
                    return;
                }
                let next = format!("{}{ch}", self.typed);
                let hits = self.matching(viewport, &next);
                if hits.is_empty() {
                    return;
                }
                self.typed = next;
                if hits.len() == 1 {
                    self.pending = Some((viewport, hits[0]));
                    self.leave(ctx);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn matching(&self, viewport: ViewportId, typed: &str) -> Vec<NodeId> {
        self.viewports
            .get(&viewport)
            .into_iter()
            .flatten()
            .filter(|target| target.hint.starts_with(typed))
            .map(|target| target.node)
            .collect()
    }

    fn keep_or_assign(&mut self, next: &mut [Target], viewport: ViewportId) {
        let previous = self.viewports.get(&viewport);
        let same_set = previous.is_some_and(|old| same_nodes(old, next));
        if same_set {
            let old = previous.expect("checked");
            for target in next.iter_mut() {
                if let Some(kept) = old.iter().find(|old| old.node == target.node) {
                    target.hint.clone_from(&kept.hint);
                }
            }
            return;
        }
        let hints = hint_strings(next.len());
        for (target, hint) in next.iter_mut().zip(hints) {
            target.hint = hint;
        }
        if !self.typed.is_empty()
            && !next
                .iter()
                .any(|target| target.hint.starts_with(&self.typed))
        {
            self.typed.clear();
        }
    }

    fn paint(&self, ctx: &Context) {
        let Some(targets) = self.viewports.get(&ctx.viewport_id()) else {
            return;
        };
        let mut visible: Vec<&Target> = targets
            .iter()
            .filter(|target| target.hint.starts_with(&self.typed))
            .collect();
        if visible.is_empty() {
            return;
        }
        let turn = self.stack % visible.len();
        visible.rotate_left(turn);
        let painter = ctx.layer_painter(LayerId::new(Order::Debug, Id::new("link-hints")));
        let screen = ctx.viewport_rect();
        for target in visible {
            paint_label(&painter, screen, target, &self.typed);
        }
    }
}

fn paint_label(painter: &Painter, screen: Rect, target: &Target, typed: &str) {
    let shown = target.hint.to_ascii_uppercase();
    let matched = typed.len().min(shown.len());
    let font = FontId::new(12.0, FontFamily::Proportional);
    let mut job = LayoutJob::default();
    if matched > 0 {
        job.append(
            &shown[..matched],
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: MATCHED,
                ..Default::default()
            },
        );
    }
    job.append(
        &shown[matched..],
        0.0,
        TextFormat {
            font_id: font,
            color: INK,
            ..Default::default()
        },
    );
    let galley = painter.layout_job(job);
    let pad = Vec2::new(3.0, 1.0);
    // Vimium anchors the label on the control's top-left corner, over the
    // control, so a button at the top of the window still keeps its letters.
    let mut origin = target.rect.left_top();
    let size = galley.size() + pad * 2.0;
    origin.x = origin
        .x
        .clamp(screen.left(), (screen.right() - size.x).max(screen.left()));
    origin.y = origin
        .y
        .clamp(screen.top(), (screen.bottom() - size.y).max(screen.top()));
    let rect = Rect::from_min_size(origin, size);
    painter.rect_filled(
        rect.translate(Vec2::new(1.0, 1.0)),
        CornerRadius::same(2),
        Color32::from_black_alpha(70),
    );
    painter.rect_filled(rect, CornerRadius::same(2), FILL);
    painter.rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0, EDGE),
        StrokeKind::Inside,
    );
    painter.galley(rect.min + pad, galley, INK);
}

/// Clickable nodes a person can see, top to bottom and then left to right.
fn controls<'a>(nodes: impl Iterator<Item = (NodeId, &'a Node)>, screen: Rect) -> Vec<Target> {
    let mut targets = Vec::new();
    for (node, data) in nodes {
        if data.is_hidden() || data.is_disabled() || !data.supports_action(Action::Click) {
            continue;
        }
        if is_chrome(data.role()) {
            continue;
        }
        let Some(bounds) = data.bounds() else {
            continue;
        };
        let Some(rect) = visible_rect(bounds, screen) else {
            continue;
        };
        let duplicate = targets.iter().any(|old: &Target| {
            (old.rect.min - rect.min).length() < 1.5
                && (old.rect.size() - rect.size()).length() < 2.0
        });
        if duplicate {
            continue;
        }
        targets.push(Target {
            node,
            rect,
            hint: String::new(),
        });
    }
    targets.sort_by(|a, b| {
        let row = |rect: Rect| (rect.min.y / 4.0).floor() as i32;
        row(a.rect)
            .cmp(&row(b.rect))
            .then_with(|| a.rect.min.x.total_cmp(&b.rect.min.x))
    });
    targets
}

fn visible_rect(bounds: NodeRect, screen: Rect) -> Option<Rect> {
    let rect = Rect::from_min_max(
        Pos2::new(bounds.x0 as f32, bounds.y0 as f32),
        Pos2::new(bounds.x1 as f32, bounds.y1 as f32),
    );
    if !rect.is_positive() || !rect.is_finite() {
        return None;
    }
    let visible = rect.intersect(screen);
    if visible.width() < 6.0 || visible.height() < 6.0 {
        return None;
    }
    let screen_area = screen.width() * screen.height();
    let area = rect.width() * rect.height();
    if screen_area > 0.0 && area > screen_area * 0.45 {
        return None;
    }
    Some(visible)
}

/// Containers and chrome. Their click, when they have one, is the frame
/// around the actual buttons, and those buttons have their own nodes.
fn is_chrome(role: Role) -> bool {
    matches!(
        role,
        Role::GenericContainer
            | Role::Window
            | Role::Pane
            | Role::ScrollView
            | Role::ScrollBar
            | Role::Splitter
            | Role::Document
            | Role::RootWebArea
            | Role::Application
            | Role::Group
            | Role::Toolbar
            | Role::Menu
            | Role::MenuBar
            | Role::Paragraph
            | Role::TextRun
            | Role::ListMarker
            | Role::TitleBar
            | Role::Tooltip
            | Role::WebView
            | Role::LayoutTable
            | Role::LayoutTableRow
            | Role::LayoutTableCell
            | Role::List
            | Role::Table
            | Role::RowGroup
            | Role::TabList
            | Role::TabPanel
            | Role::RadioGroup
            | Role::Main
            | Role::Navigation
            | Role::Banner
            | Role::ContentInfo
            | Role::Region
            | Role::Section
            | Role::Header
            | Role::Footer
            | Role::Form
            | Role::Dialog
            | Role::AlertDialog
    )
}

fn same_nodes(old: &[Target], next: &[Target]) -> bool {
    if old.len() != next.len() {
        return false;
    }
    let mut left: Vec<NodeId> = old.iter().map(|target| target.node).collect();
    let mut right: Vec<NodeId> = next.iter().map(|target| target.node).collect();
    left.sort();
    right.sort();
    left == right
}

/// Vimium's alphabet hints. Characters are prepended, the shortest complete
/// set is kept, and each label is reversed after a sort. The reverse is what
/// keeps a short label from being a prefix of a longer one, so typing can
/// always finish on the last match.
fn hint_strings(count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let alphabet: Vec<char> = HINT_CHARS.chars().collect();
    let mut hints = vec![String::new()];
    let mut offset = 0;
    while hints.len() - offset < count || hints.len() == 1 {
        let hint = hints[offset].clone();
        offset += 1;
        for ch in &alphabet {
            hints.push(format!("{ch}{hint}"));
        }
    }
    let mut chosen: Vec<String> = hints.drain(offset..offset + count).collect();
    chosen.sort();
    chosen
        .into_iter()
        .map(|hint| hint.chars().rev().collect())
        .collect()
}

fn hint_letter(key: Key) -> Option<char> {
    Some(match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_strings_are_prefix_free() {
        for count in [1, 2, 14, 15, 40, 200] {
            let hints = hint_strings(count);
            assert_eq!(hints.len(), count, "{count}");
            for (i, left) in hints.iter().enumerate() {
                assert!(!left.is_empty());
                for right in hints.iter().skip(i + 1) {
                    assert!(
                        !left.starts_with(right.as_str()) && !right.starts_with(left.as_str()),
                        "{left} overlaps {right}"
                    );
                }
            }
        }
        let varied: std::collections::HashSet<char> = hint_strings(8)
            .into_iter()
            .filter_map(|hint| hint.chars().next())
            .collect();
        assert!(
            varied.len() > 1,
            "adjacent labels should not share a first letter"
        );
    }

    fn press(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn frame(
        ctx: &Context,
        clicked: &std::cell::Cell<&str>,
        events: Vec<Event>,
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Alpha").clicked() {
                        clicked.set("alpha");
                    }
                    if ui.button("Beta").clicked() {
                        clicked.set("beta");
                    }
                });
            },
        );
        output.textures_delta.clear();
        output
    }

    #[test]
    fn typing_a_hints_letters_presses_that_button() {
        let ctx = Context::default();
        install(&ctx);
        let clicked = std::cell::Cell::new("");
        frame(&ctx, &clicked, vec![]);
        let output = frame(&ctx, &clicked, vec![press(Key::N)]);
        // Captured at the end of the `N` frame, from the tree that frame built.
        let hints = ctx
            .plugin::<LinkHints>()
            .lock()
            .viewports
            .get(&ViewportId::ROOT)
            .cloned()
            .expect("labels after N");
        assert!(hints.len() >= 2, "both buttons get a label");
        let tree = output
            .platform_output
            .accesskit_update
            .expect("tree while hints are open");
        let alpha = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Alpha"))
            .expect("Alpha button")
            .0;
        let hint = hints
            .iter()
            .find(|target| target.node == alpha)
            .map(|target| target.hint.clone())
            .unwrap_or_else(|| {
                let letters: Vec<&str> = hints.iter().map(|target| target.hint.as_str()).collect();
                panic!("Alpha has no label; assigned {letters:?}")
            });
        assert!(!hint.is_empty());

        let letters: Vec<Event> = hint.chars().map(|ch| press(letter_key(ch))).collect();
        frame(&ctx, &clicked, letters);
        frame(&ctx, &clicked, vec![]);
        assert_eq!(clicked.get(), "alpha");
        assert!(!ctx.plugin::<LinkHints>().lock().active);
    }

    #[test]
    fn a_text_field_keeps_n() {
        let ctx = Context::default();
        install(&ctx);
        let clicked = std::cell::Cell::new("");
        let mut text = String::from("song");
        let field = Id::new("hint-field");
        let mut draw = |events| {
            let mut output = ctx.run_ui(
                RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.add(egui::TextEdit::singleline(&mut text).id(field));
                    if ui.button("Alpha").clicked() {
                        clicked.set("alpha");
                    }
                },
            );
            output.textures_delta.clear();
        };
        draw(vec![]);
        ctx.memory_mut(|memory| memory.request_focus(field));
        draw(vec![]);
        draw(vec![press(Key::N)]);
        assert!(
            !ctx.plugin::<LinkHints>().lock().active,
            "N types into the field instead of opening hints"
        );
        assert_eq!(clicked.get(), "");
    }

    fn letter_key(ch: char) -> Key {
        match ch {
            'a' => Key::A,
            'b' => Key::B,
            'c' => Key::C,
            'd' => Key::D,
            'e' => Key::E,
            'f' => Key::F,
            'g' => Key::G,
            'h' => Key::H,
            'i' => Key::I,
            'j' => Key::J,
            'k' => Key::K,
            'l' => Key::L,
            'm' => Key::M,
            'n' => Key::N,
            'o' => Key::O,
            'p' => Key::P,
            'q' => Key::Q,
            'r' => Key::R,
            's' => Key::S,
            't' => Key::T,
            'u' => Key::U,
            'v' => Key::V,
            'w' => Key::W,
            'x' => Key::X,
            'y' => Key::Y,
            'z' => Key::Z,
            _ => panic!("not a hint letter: {ch}"),
        }
    }

    #[test]
    fn escape_closes_without_pressing() {
        let ctx = Context::default();
        install(&ctx);
        let clicked = std::cell::Cell::new("");
        frame(&ctx, &clicked, vec![press(Key::N)]);
        frame(&ctx, &clicked, vec![press(Key::Escape)]);
        assert!(!ctx.plugin::<LinkHints>().lock().active);
        assert_eq!(clicked.get(), "");
    }
}
