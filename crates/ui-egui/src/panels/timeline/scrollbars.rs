use super::layout::{center_scroll_for_zoom, timeline_extent, zoom_pps_limits};
use super::{Align2, Color32, CursorIcon, Drag, FilmcraftApp, MinimapCache, Pos2, Rect, Sense, Stroke, StrokeKind, Tokens, pos2, vec2};
use crate::icons::{self, Icon};
use crate::widgets::{HotNumberOptions, hot_number_numeric};
use filmcraft_project::{Sequence, TrackItem};
use filmcraft_time::Tick;
use std::collections::HashMap;

/// The narrowest the zoom scroll bar's thumb gets: room for its two handles and a part between
/// them to grab.
const MIN_THUMB_W: f32 = 34.0;

/// The minimap's default indicator colours, used when a stored one is missing or malformed.
const DEFAULT_POS_COLOR: Color32 = Color32::from_rgb(0x58, 0x95, 0xec);
const DEFAULT_VIDEO_COLOR: Color32 = Color32::from_rgb(0x64, 0xb5, 0xf6);
const DEFAULT_AUDIO_COLOR: Color32 = Color32::from_rgb(0x2e, 0x7d, 0x32);
const DEFAULT_OFFLINE_COLOR: Color32 = Color32::from_rgb(0xd3, 0x2f, 0x2f);
const MINIMAP_MAX_COLUMNS: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MinimapCue {
    None,
    Audio,
    Offline,
}

fn minimap_cue(class: u8, prioritise_audio: bool) -> MinimapCue {
    if class == 3 {
        MinimapCue::Offline
    } else if (class == 2) != prioritise_audio {
        MinimapCue::None
    } else {
        MinimapCue::Audio
    }
}

fn minimap_column_count(width: f32) -> usize {
    if width.is_finite() { width.ceil().clamp(1.0, MINIMAP_MAX_COLUMNS as f32) as usize } else { 1 }
}

#[cfg(test)]
fn clip_crosses_cut(start: Tick, end: Tick, cut: Tick) -> bool {
    start < cut && cut < end
}

fn cancel_scroll_animation_for_manual_drag(view: &mut crate::state::TimelineView) {
    view.target_scroll = view.scroll;
    view.target_pps = view.pps;
}

fn finish_minimap_cuts_strict(
    candidates: &[(usize, u64, u8, f32, Tick)],
    seq: &Sequence,
    project: &filmcraft_project::Project,
    cols: usize,
    prioritise_audio: bool,
) -> Vec<usize> {
    let media_class = |item: &TrackItem, is_audio: bool| {
        let offline = project.item(item.item).and_then(|item| item.as_media()).is_some_and(|media| media.offline);
        if offline {
            3u8
        } else if is_audio == prioritise_audio {
            2
        } else {
            1
        }
    };
    // Sweep cuts and clip intervals together. The previous per-cut scan was O(cuts * clips) on
    // every minimap draw, even though each clip interval only needs to enter and leave once.
    let mut intervals = Vec::new();
    for (tracks, is_audio) in [(&seq.video_tracks, false), (&seq.audio_tracks, true)] {
        for track in tracks {
            for item in &track.items {
                let end = item.end();
                if item.start < end {
                    intervals.push((item.start, end, track.id.0, media_class(item, is_audio)));
                }
            }
        }
    }
    intervals.sort_by_key(|(start, end, _, _)| (*start, *end));
    let mut by_end: Vec<usize> = (0..intervals.len()).collect();
    by_end.sort_by_key(|&index| intervals.get(index).map(|interval| interval.1).unwrap_or(Tick(i64::MAX)));
    let mut ordered = candidates.to_vec();
    ordered.sort_by_key(|candidate| candidate.4);
    let mut active_by_track = HashMap::<u64, [usize; 4]>::new();
    let mut active_tracks_by_class = [0usize; 4];
    let (mut next_start, mut next_end) = (0usize, 0usize);
    let mut cuts = Vec::new();
    for &(_, track_id, class, x, cut) in &ordered {
        while intervals.get(next_start).is_some_and(|interval| interval.0 < cut) {
            if let Some((_, _, track, level)) = intervals.get(next_start).copied() {
                update_active_minimap_track(&mut active_by_track, &mut active_tracks_by_class, track, level, true);
            }
            next_start = next_start.saturating_add(1);
        }
        while by_end.get(next_end).and_then(|index| intervals.get(*index)).is_some_and(|interval| interval.1 <= cut) {
            if let Some((_, _, track, level)) = by_end.get(next_end).and_then(|index| intervals.get(*index)).copied() {
                update_active_minimap_track(&mut active_by_track, &mut active_tracks_by_class, track, level, false);
            }
            next_end = next_end.saturating_add(1);
        }
        let own = active_by_track.get(&track_id).copied().unwrap_or([0; 4]);
        let covered = (usize::from(class)..active_tracks_by_class.len())
            .any(|level| active_tracks_by_class.get(level).copied().unwrap_or(0) > own.get(level).copied().unwrap_or(0));
        if !covered {
            cuts.push(((x.floor() as isize).clamp(1, (cols - 1) as isize)) as usize);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

fn update_active_minimap_track(active: &mut HashMap<u64, [usize; 4]>, totals: &mut [usize; 4], track: u64, class: u8, adding: bool) {
    let index = usize::from(class);
    let counts = active.entry(track).or_insert([0; 4]);
    let Some(count) = counts.get_mut(index) else { return };
    if adding {
        if *count == 0
            && let Some(total) = totals.get_mut(index)
        {
            *total = total.saturating_add(1);
        }
        *count = count.saturating_add(1);
    } else if *count > 0 {
        *count -= 1;
        if *count == 0
            && let Some(total) = totals.get_mut(index)
        {
            *total = total.saturating_sub(1);
        }
        if counts.iter().all(|count| *count == 0) {
            active.remove(&track);
        }
    }
}

fn minimap_coverage(seq: &Sequence, project: &filmcraft_project::Project, span: f64, width: f32, cols: usize, prioritise_audio: bool) -> (Vec<u8>, Vec<usize>) {
    let mut class = vec![0u8; cols];
    let x_of = |time: f64| {
        let time = if time.is_finite() { time.clamp(0.0, span) } else { 0.0 };
        (time / span * f64::from(width)) as f32
    };
    let media_class = |item: &TrackItem, is_audio: bool| {
        let offline = project.item(item.item).and_then(|item| item.as_media()).is_some_and(|media| media.offline);
        if offline {
            3u8
        } else if is_audio == prioritise_audio {
            2
        } else {
            1
        }
    };
    let mut adjacent_cuts = Vec::new();
    for (tracks, is_audio) in [(&seq.video_tracks, false), (&seq.audio_tracks, true)] {
        for track in tracks {
            for item in &track.items {
                let item_class = media_class(item, is_audio);
                let start = x_of(item.start.seconds());
                let end = x_of(item.end().seconds());
                let first = ((start.floor() as isize).max(0) as usize).min(cols);
                let last = ((end.ceil() as isize).clamp(0, cols as isize) as usize).min(cols);
                for cell in first..last {
                    if let Some(value) = class.get_mut(cell) {
                        *value = (*value).max(item_class);
                    }
                }
            }
            for pair in track.items.windows(2) {
                let Some((left, right)) = pair.first().zip(pair.get(1)) else { continue };
                if left.end() != right.start || media_class(left, is_audio) != media_class(right, is_audio) {
                    continue;
                }
                let x = x_of(left.end().seconds());
                let cell = ((x.floor() as isize).clamp(0, cols.saturating_sub(1) as isize)) as usize;
                if cols > 1 {
                    adjacent_cuts.push((cell, track.id.0, media_class(left, is_audio), x, left.end()));
                }
            }
        }
    }
    let cuts = finish_minimap_cuts_strict(&adjacent_cuts, seq, project, cols, prioritise_audio);
    (class, cuts)
}

fn minimap_runs(class: &[u8], cuts: &[usize]) -> Vec<(usize, usize, u8)> {
    let mut runs = Vec::new();
    let mut c = 0;
    while c < class.len() {
        if class[c] == 0 {
            c += 1;
            continue;
        }
        let k = class[c];
        let start = c;
        c += 1;
        while c < class.len() && class[c] == k {
            if cuts.binary_search(&c).is_ok() {
                break;
            }
            c += 1;
        }
        runs.push((start, c, k));
    }
    runs
}

/// The Maximum Zoom Level, sanitized: the least of the Timeline the view may show, 0.05-3 s
/// (the setting; hostile values give the 0.4 s default).
fn min_view_secs(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 { v.clamp(0.05, 3.0) } else { 0.4 }
}

/// The log-mapped share of the zoom range: 0.0 when the view shows `min_view` seconds (the
/// Maximum Zoom Level setting -- zoomed all the way in), 1.0 when it shows `vis_max` seconds (the
/// whole Timeline; the thumb fills the bar). The thumb's width is this fraction of the span between
/// [`MIN_THUMB_W`] and the bar's width: an
/// exponential zoom meter, so the thumb can keep shrinking to the smallest view no matter how long
/// the Timeline is, while a Premiere-style linear fraction would bottom out (a 34-point thumb at
/// ~28 seconds into a 10-minute Timeline).
fn log_frac(vis: f64, vis_max: f64, min_view: f64) -> f64 {
    let min_view = min_view_secs(min_view);
    let span = (vis_max / min_view).ln();
    if vis.is_finite() && vis > min_view && span > 1e-9 { ((vis / min_view).ln() / span).clamp(0.0, 1.0) } else { 0.0 }
}

/// The seconds in view for a [`log_frac`] fraction of the zoom range.
fn vis_at_frac(f: f64, vis_max: f64, min_view: f64) -> f64 {
    let min_view = min_view_secs(min_view);
    let span = (vis_max / min_view).ln();
    if f.is_finite() && f >= 0.0 && span > 1e-9 { min_view * (f * span.clamp(0.0, 709.0)).exp().min(vis_max / min_view) } else { vis_max }
}

/// Where the zoom scroll bar's thumb is: (left edge, width, seconds of scroll per point of thumb
/// travel). The thumb's width is the log-mapped zoom share ([`log_frac`]) of the span between
/// [`MIN_THUMB_W`] and the bar's full width: exactly [`MIN_THUMB_W`] at the Maximum Zoom Level --
/// the slider's smallest state, reached only there and never before, however long the Timeline is
/// -- and filling the bar when the whole Timeline is visible, shrinking steadily in between. Its
/// position maps the scroll range linearly, so it reaches both ends of the bar and moves with the
/// pointer when dragged. `view_w` is the tracks' content width: the width, in points, that `pps`
/// maps to (differing from the bar's own width by a few points).
fn zoom_thumb(bar: Rect, view_w: f32, seq_seconds: f64, scroll: f64, pps: f64, min_view: f64) -> (f32, f32, f64) {
    let vis = view_w.max(1.0) as f64 / pps.max(1e-9);
    let total = timeline_extent(seq_seconds).max(vis);
    let f = log_frac(vis, total, min_view) as f32;
    let w = (MIN_THUMB_W + f * (bar.width() - MIN_THUMB_W)).clamp(MIN_THUMB_W.min(bar.width()), bar.width());
    let travel = (bar.width() - w).max(0.0);
    let range = (total - vis).max(0.0);
    let at = if range > 0.0 { (scroll / range).clamp(0.0, 1.0) as f32 * travel } else { 0.0 };
    (bar.min.x + at, w, if travel > 0.5 { range / travel as f64 } else { 0.0 })
}

/// Grows or shrinks the view symmetrically about its center: sets a new points-a-second and moves
/// the scroll so the footage at the view's middle stays put. Both ends of the zoom-bar thumb move
/// together like a mirror -- this is how the edge handles and the wheel zoom, and it never lets the
/// scroll escape the timeline. `new_pps` is clamped into (0.05, [`max_pps`] of `view_w`) first.
fn zoom_about_center(v: &mut crate::state::TimelineView, view_w: f32, sequence_seconds: f64, total: f64, new_pps: f64, min_view: f64) {
    if !(new_pps.is_finite() && new_pps > 0.0) {
        return;
    }
    // the widest zoom is the sequence-dependent floor (the whole Timeline in view), never a
    // fixed constant: a long sequence must be able to zoom out to its full extent
    let (floor, ceiling) = zoom_pps_limits(sequence_seconds, view_w, min_view);
    let new_pps = new_pps.clamp(floor, ceiling);
    let old_pps = v.pps;
    let old_scroll = v.scroll;
    v.pps = new_pps;
    v.target_pps = new_pps;
    v.scroll = center_scroll_for_zoom(old_scroll, old_pps, new_pps, view_w, total);
    v.target_scroll = v.scroll;
}

/// The zoom scroll bar's settings popup content. Opened by right-clicking the bar, and by the
/// Timeline menu's "Timeline Slider Settings" row while the bar is hidden -- the same settings,
/// the same way, in both places. The popup only becomes scrollable when the window is too small
/// to show it whole (the height cap is the screen's, so at a normal window size nothing changes).
pub(super) fn zoombar_settings(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    // Scrollable only in an emergency: the scroll area is given a whole screen of room (a
    // popup's own ui only offers the space left below its anchor, which forced a scrollbar and
    // cut the menu at a normal window size). On any window big enough for the menu nothing
    // changes and no scrollbar ever appears.
    // Keep the scroll viewport tied to the live window size. A fixed minimum makes the area
    // larger than the viewport in short windows, so egui cannot update the visible rows smoothly
    // as the user resizes the window.
    let max_h = settings_popup_max_height(ui.ctx().content_rect().height());
    let r = ui.available_rect_before_wrap();
    let r = Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + max_h));
    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
        let output = egui::ScrollArea::vertical().id_salt("timeline-zoombar-settings-scroll").max_height(max_h).show(ui, |ui| {
            zoombar_settings_rows(app, ui);
        });
        ui.ctx().data_mut(|data| data.insert_temp(zoombar_popup_content_height_key(), output.content_size.y));
    });
}

fn settings_popup_max_height(viewport_height: f32) -> f32 {
    (viewport_height - 16.0).max(1.0)
}

fn zoombar_popup_content_height_key() -> egui::Id {
    egui::Id::unique("timeline-zoombar-popup-content-height")
}

/// Draw the right-click popup at its natural height. Hide the scrollbar when the full content fits
/// the window; only cap the content and allow scrolling when it cannot fit the window at all.
fn zoombar_popup_content(app: &mut FilmcraftApp, ui: &mut egui::Ui, screen_height: f32, measuring: bool) {
    let original_inactive = ui.visuals().widgets.inactive.bg_fill;
    ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_gray(100);
    let content_height = ui.ctx().data(|data| data.get_temp::<f32>(zoombar_popup_content_height_key())).unwrap_or(0.0);
    let needs_scroll = !measuring && content_height + 16.0 > screen_height;
    let fresh_key = egui::Id::unique("timeline-zoombar-popup-fresh");
    if measuring {
        ui.ctx().data_mut(|data| data.insert_temp(fresh_key, true));
    }
    if !needs_scroll {
        ui.visuals_mut().widgets.inactive.bg_fill = original_inactive;
        let rows = ui.scope(|ui| zoombar_settings_rows(app, ui));
        if measuring {
            ui.ctx().data_mut(|data| data.insert_temp(zoombar_popup_content_height_key(), rows.response.rect.height()));
        }
        return;
    }
    let fresh = ui.ctx().data(|data| data.get_temp::<bool>(fresh_key)).unwrap_or(false);
    if measuring {
        ui.ctx().data_mut(|data| data.insert_temp(fresh_key, true));
    } else if fresh {
        ui.ctx().data_mut(|data| data.insert_temp(fresh_key, false));
    }
    let scroll = egui::ScrollArea::vertical().id_salt("timeline-zoombar-context-popup").scroll_bar_visibility(if needs_scroll {
        egui::containers::scroll_area::ScrollBarVisibility::VisibleWhenNeeded
    } else {
        egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden
    });
    let scroll = if needs_scroll {
        let scroll = scroll.max_height((screen_height - 32.0).max(1.0));
        if fresh { scroll.vertical_scroll_offset(content_height) } else { scroll }
    } else {
        scroll
    };
    let output = scroll.show(ui, |ui| {
        ui.visuals_mut().widgets.inactive.bg_fill = original_inactive;
        zoombar_settings_rows(app, ui);
    });
    let _ = output;
}

fn zoombar_popup_open_key() -> egui::Id {
    egui::Id::unique("timeline-zoombar-settings-open")
}

/// The rows of the zoom bar's settings popup (see [`zoombar_settings`]).
// one section per setting keeps the popup's rows in visual order; splitting the function would
// scatter the popup's layout over the file
#[allow(clippy::cognitive_complexity)]
fn zoombar_settings_rows(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    /// The settings popup's inner content width. Fixed, never derived from the frame's layout, so
    /// widgets measure identically on the popup's first frame and forever after (a first-frame
    /// mismatch is what made the popup jitter).
    const CONTENT_W: f32 = 257.4;
    ui.set_max_width(CONTENT_W + 16.0);
    let mut changed = false;
    family_reset(ui);
    capture_controls(app, ui);
    let content_w = CONTENT_W.min(ui.available_width());

    // The very top row: Hide Slider (red) while the bar is showing -- pressing it hides the bar at
    // once and takes this popup with it, and the window that follows only tells the user how to
    // bring the bar back. While the bar is hidden (this popup opened from the Timeline's own menu)
    // the row is a BLUE Unhide Slider instead; that one never exists in the bar's own popup.
    // The popup's content right edge (the row spans the whole content width) -- the Minimap flyout
    // is placed beside the popup, so it needs to know where the popup ends.
    let hidden = app.session.state.timeline_ui.zoom_bar_hidden;
    let mut content_right = 0.0f32;
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        let row_w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(egui::vec2(row_w, 30.0), Sense::click());
        content_right = r.max.x;
        let hide_resp =
            ui.interact(r, ui.make_persistent_id(if hidden { "timeline-zoombar-unhide-slider" } else { "timeline-zoombar-hide-slider" }), Sense::click());
        if hide_resp.hovered() {
            ui.painter().rect_filled(r, 4.0, app.tokens.switch_border.gamma_multiply(0.35));
        }
        let (label, color) = if hidden { (tl!("Unhide Slider"), app.tokens.accent) } else { (tl!("Hide Slider"), Color32::from_rgb(0xf8, 0x7e, 0x7a)) };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, label, Tokens::ui(11.5), color);
        app.auto.add(if hidden { "timeline.zoomBar.settings.unhideSlider" } else { "timeline.zoomBar.settings.hideSlider" }, r, label);
        if hide_resp.clicked() {
            set_zoombar_hidden(app, ui.ctx(), !hidden);
            if hidden {
                app.tl.zoombar_settings_pos = None;
            } else {
                app.tl.zoombar_hide_dialog_open = true;
            }
            ui.close();
            ui.ctx().request_repaint();
        }
    });
    // "Controls": not a switch -- a list of the slider's gestures, in a flyout beside the menu
    // (the Minimap Settings flyout's exact recipe). The text centers like Hide Slider's; the
    // arrow stays where it was.
    let full_w = ui.available_width();
    let (row, _) = ui.allocate_exact_size(egui::vec2(full_w, 30.0), Sense::hover());
    let label = tl!("Controls");
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), app.tokens.text);
    ui.painter().galley(pos2(row.center().x - galley.size().x / 2.0, row.center().y - galley.size().y / 2.0), galley, app.tokens.text);
    // the arrow points at whichever side the flyout fits on, the same distance from that wall
    controls_arrow(ui, row, content_w, content_right, flyout_to_the_right(ui, row, content_right, CONTROLS_FLYOUT_W), app.tokens.text_dim);
    app.auto.add("timeline.zoomBar.settings.controls", row, "Controls");
    hover_flyout(
        app,
        ui,
        ui.make_persistent_id("timeline-zoombar-advanced-flyout"),
        row,
        content_right,
        CONTROLS_FLYOUT_W,
        true,
        "timeline.zoomBar.settings.controlsFlyout",
        "Controls",
        &[
            (tl!("Center on Playhead:"), tl!("Double Click").to_string(), 0),
            (tl!("Explode Slider:"), tl!("Triple Click").to_string(), 1),
            (tl!("Alternate Drag Speed:"), tl!("Shift + Drag").to_string(), 2),
            (tl!("Settings:"), tl!("Right Click").to_string(), 3),
        ],
    );
    let sequence_seconds = app.session.active_sequence().map(|seq| seq.duration().seconds()).unwrap_or(0.0);
    let settings = &mut app.session.state.timeline_ui;

    // Smooth Animations: a centered pill switch (left) + label group instead of a checkbox
    let (row, _) = ui.allocate_exact_size(egui::vec2(content_w, 30.0), Sense::hover());
    let t = &app.tokens;
    let label = tl!("Smooth Animations");
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    let sw_w = 34.0;
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((content_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, settings.smooth_animations, t, ui.make_persistent_id("timeline-zoombar-smooth"), label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, t.text);
    app.auto.add("timeline.zoomBar.settings.smoothAnimations", c.rect, "Smooth Animations");
    if c.clicked() {
        settings.smooth_animations = !settings.smooth_animations;
        changed = true;
    }
    // Inverted Controls: the wheel over the tracks scrolls the Timeline the other way in time,
    // and the wheel while grabbing the slider's middle zooms the other way
    let (row, _) = ui.allocate_exact_size(egui::vec2(content_w, 30.0), Sense::hover());
    let label = tl!("Invert Scrolling");
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((content_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, settings.inverted_controls, t, ui.make_persistent_id("timeline-zoombar-inverted"), label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, t.text);
    app.auto.add("timeline.zoomBar.settings.invertedControls", c.rect, "Invert Scrolling");
    if c.clicked() {
        settings.inverted_controls = !settings.inverted_controls;
        changed = true;
    }
    // Playhead Centering: same centered pill switch + label group as the row above it
    let (row, _) = ui.allocate_exact_size(egui::vec2(content_w, 30.0), Sense::hover());
    let label = tl!("Playhead Centering");
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((content_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, settings.playhead_centering, t, ui.make_persistent_id("timeline-zoombar-center"), label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, t.text);
    app.auto.add("timeline.zoomBar.settings.playheadCentering", c.rect, "Playhead Centering");
    if c.clicked() {
        settings.playhead_centering = !settings.playhead_centering;
        if !settings.playhead_centering {
            app.tl.playhead_captured = false;
            app.tl.playhead_capture_vis = 0.0;
        }
        changed = true;
    }
    // Alternate Slider Position: same centered pill switch + label group as the rows above it
    let (row, _) = ui.allocate_exact_size(egui::vec2(content_w, 30.0), Sense::hover());
    let label = tl!("Alternate Slider Position");
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((content_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, settings.alternate_slider_position, t, ui.make_persistent_id("timeline-zoombar-alternate"), label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, t.text);
    app.auto.add("timeline.zoomBar.settings.alternateSliderPosition", c.rect, "Alternate Slider Position");
    if c.clicked() {
        settings.alternate_slider_position = !settings.alternate_slider_position;
        changed = true;
    }
    ui.separator();
    // Maximum Zoom Level: the smallest view the zoom bar may collapse to
    settings_heading(ui, t, tl!("Maximum Zoom Level"));
    let zoom_presets = [0.1, 0.2, 0.4, 0.8];
    let mut zoom_choice =
        if settings.max_zoom_custom { 4 } else { zoom_presets.iter().position(|v| (*v - settings.max_zoom_seconds).abs() < f64::EPSILON).unwrap_or(2) };
    let zoom_labels = [tl!("Closest"), tl!("Close"), tl!("Default"), tl!("Far"), tl!("Custom")];
    let (r, moved) = ui
        .allocate_ui_with_layout(egui::vec2(content_w, 46.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            tick_slider(ui, &mut zoom_choice, &zoom_labels, &app.tokens)
        })
        .inner;
    changed |= moved;
    app.auto.add("timeline.zoomBar.settings.maxZoomLevel.slider", r.rect, "Maximum zoom level");
    if moved {
        apply_max_zoom_choice(settings, zoom_choice, &zoom_presets);
    }
    // A constant, always-visible and editable value row, centered like the sliders. It keeps the
    // popup's height fixed, so the layout is identical on the popup's first frame and forever after.
    ui.allocate_ui_with_layout(egui::vec2(content_w, 24.0), egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.add_sized(egui::vec2(46.0, 18.0), egui::Label::new(tl!("Value:")).wrap());
        let (response, value) = hot_number_numeric(
            ui,
            tl!("Maximum zoom level in seconds"),
            ui.make_persistent_id("timeline-zoombar-max-zoom"),
            settings.max_zoom_seconds,
            HotNumberOptions::new(0.05, (0.05, 3.0), 2, "s", &app.tokens),
        );
        app.auto.add("timeline.zoomBar.settings.maxZoomLevel", response.rect, "Maximum zoom level in seconds");
        if let Some(value) = value {
            settings.max_zoom_seconds = value;
            settings.max_zoom_last_custom = value;
            settings.max_zoom_custom = true;
            changed = true;
        }
    });
    // a zoom established under the old ceiling must obey the new one at once: the same cap the
    // control channel applies when `maxZoomLevel` changes
    if changed && app.session.state.active_sequence.is_some() && app.last_timeline_width > 50.0 {
        let width = app.last_timeline_width;
        let extent = timeline_extent(sequence_seconds);
        let (floor, cap) = zoom_pps_limits(sequence_seconds, width, settings.max_zoom_secs());
        let view = &mut app.ui.timeline;
        let old_target_pps = view.target_pps;
        let target_pps = if old_target_pps.is_finite() && old_target_pps > 0.0 { old_target_pps.clamp(floor, cap) } else { cap };
        let old_pps = view.pps;
        let pps = if old_pps.is_finite() && old_pps > 0.0 { old_pps.clamp(floor, cap) } else { cap };
        let target_clamped = target_pps < old_target_pps;
        let pps_clamped = pps < old_pps;
        if target_clamped {
            view.target_scroll = center_scroll_for_zoom(view.target_scroll, old_target_pps, target_pps, width, extent);
        }
        if pps_clamped {
            view.scroll = center_scroll_for_zoom(view.scroll, old_pps, pps, width, extent);
        }
        view.target_pps = target_pps;
        view.pps = pps;
        if target_clamped || pps_clamped {
            app.tl.zoom_anchor = None;
        }
    }
    ui.separator();
    // Alternate Drag Speed first
    // the gesture is part of the heading only while it is still the way to do it: a Controls
    // rebinding makes it disappear
    let alt_speed_label =
        if settings.control_alternate_drag_speed.is_empty() { tl!("Alternate Drag Speed (Shift + Left Click)") } else { tl!("Alternate Drag Speed") };
    settings_heading(ui, t, alt_speed_label);
    let speed_presets = [0.25, 0.5, 1.0, 2.0];
    let mut speed_choice =
        if settings.shift_drag_custom { 4 } else { speed_presets.iter().position(|v| (*v - settings.shift_drag_speed).abs() < f64::EPSILON).unwrap_or(3) };
    let speed_labels = [tl!("Very Slow"), tl!("Slow"), tl!("Default"), tl!("Fast"), tl!("Custom")];
    let (r, moved) = ui
        .allocate_ui_with_layout(egui::vec2(content_w, 46.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            tick_slider(ui, &mut speed_choice, &speed_labels, &app.tokens)
        })
        .inner;
    changed |= moved;
    app.auto.add("timeline.zoomBar.settings.shiftDragSpeed.slider", r.rect, "Alternate drag speed");
    if moved {
        match speed_choice {
            0..=3 => {
                settings.shift_drag_speed = speed_presets[speed_choice];
                settings.shift_drag_custom = false;
            }
            _ => {
                // Custom again: restore the last custom value, never reset it to a default
                if !settings.shift_drag_custom {
                    settings.shift_drag_speed = last_custom_value(settings.shift_drag_last_custom, 50.0);
                }
                settings.shift_drag_custom = true;
            }
        }
    }
    // A constant, always-visible and editable value row, centered like the sliders. It keeps the
    // popup's height fixed, so the layout is identical on the popup's first frame and forever
    // after (a first-frame mismatch is what made the popup jitter).
    ui.allocate_ui_with_layout(egui::vec2(content_w, 24.0), egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.add_sized(egui::vec2(46.0, 18.0), egui::Label::new(tl!("Value:")).wrap());
        let (response, value) = hot_number_numeric(
            ui,
            tl!("Alternate drag speed multiplier"),
            ui.make_persistent_id("timeline-zoombar-shift-speed"),
            settings.shift_drag_speed,
            HotNumberOptions::new(0.1, (0.1, 50.0), 1, "x", &app.tokens),
        );
        app.auto.add("timeline.zoomBar.settings.shiftDragSpeed", response.rect, "Alternate drag speed multiplier");
        if let Some(value) = value {
            settings.shift_drag_speed = value;
            settings.shift_drag_last_custom = value;
            settings.shift_drag_custom = true;
            changed = true;
        }
    });
    ui.separator();
    // Playhead Centering Range second
    let range_label =
        if settings.control_center_on_playhead.is_empty() { tl!("Playhead Centering Range (Double Click)") } else { tl!("Playhead Centering Range") };
    settings_heading(ui, t, range_label);
    let mut range_choice = if settings.playhead_side_custom {
        4
    } else {
        [3.0, 5.0, 10.0, 30.0].iter().position(|v| (*v - settings.playhead_side_seconds).abs() < f64::EPSILON).unwrap_or(2)
    };
    let range_labels = [tl!("3s"), tl!("5s"), tl!("10s"), tl!("30s"), tl!("Custom")];
    let (r, moved) = ui
        .allocate_ui_with_layout(egui::vec2(content_w, 46.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            tick_slider(ui, &mut range_choice, &range_labels, &app.tokens)
        })
        .inner;
    changed |= moved;
    app.auto.add("timeline.zoomBar.settings.playheadSideSeconds.slider", r.rect, "Playhead centering range");
    if moved {
        match range_choice {
            0..=3 => {
                settings.playhead_side_seconds = [3.0, 5.0, 10.0, 30.0][range_choice];
                settings.playhead_side_custom = false;
            }
            _ => {
                // Custom again: restore the last custom value, never reset it to a default
                if !settings.playhead_side_custom {
                    settings.playhead_side_seconds = last_custom_value(settings.playhead_side_last_custom, 3600.0);
                }
                settings.playhead_side_custom = true;
            }
        }
    }
    // A constant, always-visible and editable value row, centered like the sliders, so the popup
    // never resizes.
    ui.allocate_ui_with_layout(egui::vec2(content_w, 24.0), egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.add_sized(egui::vec2(46.0, 18.0), egui::Label::new(tl!("Value:")).wrap());
        let (response, value) = hot_number_numeric(
            ui,
            tl!("Playhead centering range in seconds"),
            ui.make_persistent_id("timeline-zoombar-side"),
            settings.playhead_side_seconds,
            HotNumberOptions::new(0.1, (0.1, 3600.0), 1, "s", &app.tokens),
        );
        app.auto.add("timeline.zoomBar.settings.playheadSideSeconds", response.rect, "Playhead centering range in seconds");
        if let Some(value) = value {
            settings.playhead_side_seconds = value;
            settings.playhead_side_last_custom = value;
            settings.playhead_side_custom = true;
            changed = true;
        }
    });
    // The same faint divider as between the other sections.
    ui.separator();
    ui.add_space(2.0);
    let mut whole = settings.show_whole_timeline;
    // Slider Minimap: a pill switch like the rows above, with an arrow promising more settings --
    // hovering the row opens the Minimap Settings flyout, as wide as this menu. While the minimap
    // is OFF the label is greyed out, the arrow is gone, and hovering opens nothing.
    let minimap_on = settings.slider_minimap;
    let (row, _) = ui.allocate_exact_size(egui::vec2(content_w, 30.0), Sense::hover());
    let label = tl!("Slider Minimap");
    // off: a darker grey than the other dimmed labels, so the dead row reads as dead
    let label_color = if minimap_on { t.text } else { t.text_faint };
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), label_color);
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((content_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, minimap_on, t, ui.make_persistent_id("timeline-zoombar-minimap"), label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, label_color);
    if minimap_on {
        // the same side-aware arrow as the Controls row, at the same distance from the wall
        controls_arrow(ui, row, content_w, content_right, flyout_to_the_right(ui, row, content_right, CONTENT_W), t.text_dim);
    }
    app.auto.add("timeline.zoomBar.settings.sliderMinimap", c.rect, "Slider Minimap");
    if c.clicked() {
        settings.slider_minimap = !settings.slider_minimap;
        if !settings.slider_minimap {
            // turning the minimap off kills its flyout in the SAME click -- no grace period,
            // nothing keeps ticking for a setting that is gone
            let flyout_id = ui.make_persistent_id("timeline-zoombar-minimap-flyout");
            ui.data_mut(|d| {
                d.insert_temp(flyout_id.with("open"), false);
                d.insert_temp(grace_key(), -1e9f64);
            });
        }
        changed = true;
    }
    // Minimap Settings: a draft flyout behind the arrow, kept open while the pointer is on the
    // row, on the settings menu, in the ring of gaps around it, or in the flyout itself. A plain
    // hover panel, not a menu popup: a nested menu popup would close the settings menu it lives in.
    //
    // The keep-alive MUST use plain geometry against the raw pointer position. egui's
    // `rect_contains_pointer` is layer-bound: it reports false for the gap strips (they are over
    // the Timeline's layer) and for the flyout (its own Area, a different layer), which is exactly
    // why the flyout used to vanish the moment the pointer left the row. A 300 ms grace after the
    // last hit covers any one-frame hole in the geometry.
    let flyout_id = ui.make_persistent_id("timeline-zoombar-minimap-flyout");
    remember_flyout_id(ui.ctx(), flyout_id);
    let rect_key = flyout_id.with("rect");
    let height_key = flyout_id.with("height");
    let stamp_key = flyout_id.with("stamp");
    let mut fill = settings.fill_clips;
    let mut pri = settings.prioritise_audio;
    let mut pos_ind = settings.show_position_indicator;
    // Beside the settings menu, never on top of it: to the right when the screen has room for it
    // there, otherwise to the left of the menu. Bottom-aligned with the menu's bottom edge and
    // always fully opaque -- an egui Area fades in by default, which is what made a flipped flyout
    // look ghostly. The placement is relative to the menu only, so where the slider was
    // right-clicked cannot shift it.
    let margin = ui.spacing().menu_margin;
    let popup_right = content_right + f32::from(margin.right);
    let popup_left = row.min.x - f32::from(margin.left);
    let flyout_w = CONTENT_W + 16.0 + f32::from(margin.left) + f32::from(margin.right);
    let screen = ui.ctx().content_rect();
    let gap = 4.0;
    let (x, to_the_right) =
        if popup_right + gap + flyout_w <= screen.right() { (popup_right + gap, true) } else { ((popup_left - gap - flyout_w).max(screen.left()), false) };
    let popup_bottom = row.max.y + f32::from(margin.bottom);
    let flyout_h = ui.data(|d| d.get_temp::<f32>(height_key)).unwrap_or(110.0);
    let y = (popup_bottom - flyout_h).min(screen.bottom() - flyout_h);
    let flyout_rect = Rect::from_min_size(pos2(x, y), egui::vec2(flyout_w, flyout_h));
    // the keep-alive frame around the menu: the pointer may travel anywhere on the menu or in a
    // 24 pt ring of gaps around it (and across the gap strip to the flyout) without losing it
    let menu = Rect::from_min_max(pos2(popup_left, row.min.y - 30.0 - f32::from(margin.top)), pos2(popup_right, popup_bottom));
    let (gx0, gx1) = if to_the_right { (popup_right, x) } else { (x + flyout_w, popup_left) };
    let side = Rect::from_min_max(pos2(gx0.min(gx1), y), pos2(gx0.max(gx1), popup_bottom.max(y + flyout_h)));
    let under = Rect::from_min_max(pos2(popup_left, popup_bottom), pos2(popup_right, popup_bottom + 24.0));
    let around = Rect::from_min_max(pos2(menu.min.x - 24.0, menu.min.y - 24.0), pos2(menu.max.x + 24.0, menu.max.y + 24.0));
    let pointer = ui.input(|i| i.pointer.hover_pos().or(i.pointer.interact_pos()));
    let over = |r: Rect| pointer.is_some_and(|p| r.contains(p));
    let prev_flyout = ui.data(|d| d.get_temp::<Rect>(rect_key)).unwrap_or(flyout_rect);
    // the settings' own popups count too: a colour picker opened from this flyout hangs below it,
    // and working in it must never take the flyout (and the picker with it) away
    let now = ui.input(|i| i.time);
    // Working in the colour picker (or the three seconds after it) keeps the flyout alive no
    // matter where the pointer goes.
    let keep = ui.data(|d| d.get_temp::<f64>(grace_key())).unwrap_or(-1e9);
    // the row is live only while the minimap is on: off, hovering it opens nothing
    let row_hot = over(row) && settings.slider_minimap;
    let keep_alive = row_hot
        || over(menu)
        || over(side)
        || over(under)
        || over(around)
        || over(prev_flyout)
        || over(flyout_rect)
        || pointer.is_some_and(|p| family_contains_prev(ui, p))
        || now - keep < 3.0;
    // The ROW opens the flyout -- and only on a real hover of it. The menu appears under the
    // cursor, often right on this row, and "the menu opens default" means the flyout must never
    // pop up just because it landed there: the pointer has to move first.
    let opened_at = ui.data(|d| d.get_temp::<Pos2>(open_pointer_key())).unwrap_or(pos2(-1e9, -1e9));
    let moved = pointer.is_some_and(|p| (p - opened_at).length() > 6.0);
    let trigger = row_hot && moved;
    // everything else (the gap strips, the flyout itself, the colour pickers) only KEEPS it open;
    // the flyout never exists at all while the minimap is off
    let was_open = ui.data(|d| d.get_temp::<bool>(flyout_id.with("open"))).unwrap_or(false);
    let stamp = ui.data(|d| d.get_temp::<f64>(stamp_key)).unwrap_or(-1e9);
    let open = settings.slider_minimap && (trigger || (was_open && (keep_alive || now - stamp < 0.3)));
    if trigger || keep_alive {
        ui.data_mut(|d| d.insert_temp(stamp_key, now));
    }
    ui.data_mut(|d| d.insert_temp(flyout_id.with("open"), open));
    if open {
        let panel = egui::Area::new(flyout_id).order(egui::Order::Foreground).fixed_pos(pos2(x, y)).fade_in(false).interactable(true).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(CONTENT_W + 16.0);
                ui.set_max_width(CONTENT_W + 16.0);
                // Show Whole Timeline: with it on the minimap is locked to the full Timeline
                // extent, so the "you are here" line can travel past the last clip.
                let (frow, _) = ui.allocate_exact_size(egui::vec2(CONTENT_W, 30.0), Sense::hover());
                let flabel = tl!("Show Whole Timeline");
                let fgalley = ui.painter().layout_no_wrap(flabel.to_string(), Tokens::ui(11.5), t.text);
                let fsw_w = 34.0;
                let fgroup_w = fsw_w + 10.0 + fgalley.size().x;
                let fx0 = frow.min.x + ((CONTENT_W - fgroup_w) / 2.0).max(0.0);
                let fsw = Rect::from_min_size(pos2(fx0, frow.center().y - 8.0), egui::vec2(fsw_w, 16.0));
                let c = pill_switch(ui, fsw, whole, t, ui.make_persistent_id("timeline-zoombar-minimap-whole"), flabel);
                ui.painter().galley(pos2(fx0 + fsw_w + 10.0, frow.center().y - fgalley.size().y / 2.0), fgalley, t.text);
                app.auto.add("timeline.zoomBar.settings.showWholeTimeline", c.rect, "Show Whole Timeline");
                if c.clicked() {
                    whole = !whole;
                }
                // Prioritise Audio: where an audio clip and a video clip sit at the same place
                // the audio shows on top of the video instead of under it.
                let (prow, _) = ui.allocate_exact_size(egui::vec2(CONTENT_W, 30.0), Sense::hover());
                let plabel = tl!("Prioritise Audio");
                let pgalley = ui.painter().layout_no_wrap(plabel.to_string(), Tokens::ui(11.5), t.text);
                let pgroup_w = fsw_w + 10.0 + pgalley.size().x;
                let px0 = prow.min.x + ((CONTENT_W - pgroup_w) / 2.0).max(0.0);
                let psw = Rect::from_min_size(pos2(px0, prow.center().y - 8.0), egui::vec2(fsw_w, 16.0));
                let c = pill_switch(ui, psw, pri, t, ui.make_persistent_id("timeline-zoombar-minimap-pri-audio"), plabel);
                ui.painter().galley(pos2(px0 + fsw_w + 10.0, prow.center().y - pgalley.size().y / 2.0), pgalley, t.text);
                app.auto.add("timeline.zoomBar.settings.prioritiseAudio", c.rect, "Prioritise Audio");
                if c.clicked() {
                    pri = !pri;
                }
                // Position Indicator: the "you are here" line on the slider. Off: hidden.
                let c = settings_pill_row(ui, t, CONTENT_W, tl!("Position Indicator"), ui.make_persistent_id("timeline-zoombar-minimap-pos-ind"), pos_ind);
                app.auto.add("timeline.zoomBar.settings.positionIndicator", c.rect, "Position Indicator");
                if c.clicked() {
                    pos_ind = !pos_ind;
                }
                // Fill Clips: on, the minimap's clips are filled in; off (default), outlines.
                ui.separator();
                let (grow, _) = ui.allocate_exact_size(egui::vec2(CONTENT_W, 30.0), Sense::hover());
                let glabel = tl!("Fill Clips");
                let ggalley = ui.painter().layout_no_wrap(glabel.to_string(), Tokens::ui(11.5), t.text);
                let ggroup_w = fsw_w + 10.0 + ggalley.size().x;
                let gxp = grow.min.x + ((CONTENT_W - ggroup_w) / 2.0).max(0.0);
                let gsw = Rect::from_min_size(pos2(gxp, grow.center().y - 8.0), egui::vec2(fsw_w, 16.0));
                let c = pill_switch(ui, gsw, fill, t, ui.make_persistent_id("timeline-zoombar-minimap-fill"), glabel);
                ui.painter().galley(pos2(gxp + fsw_w + 10.0, grow.center().y - ggalley.size().y / 2.0), ggalley, t.text);
                app.auto.add("timeline.zoomBar.settings.fillClips", c.rect, "Fill Clips");
                if c.clicked() {
                    fill = !fill;
                }
                // Position Indicator Color: the colour of the "you are here" line. One call --
                // copy this line for every future colour setting.
                changed |= color_setting(
                    ui,
                    t,
                    &mut ColorAutomation { registry: &mut app.auto, prefix: "timeline.zoomBar.settings.positionIndicatorColor" },
                    ui.make_persistent_id("timeline-zoombar-minimap-color"),
                    tl!("Position Indicator Color:"),
                    &mut settings.position_indicator_color,
                    "#5895ec",
                );
                changed |= color_setting(
                    ui,
                    t,
                    &mut ColorAutomation { registry: &mut app.auto, prefix: "timeline.zoomBar.settings.videoIndicatorColor" },
                    ui.make_persistent_id("timeline-zoombar-minimap-video-color"),
                    tl!("Video Indicator Color:"),
                    &mut settings.video_indicator_color,
                    "#64b5f6",
                );
                changed |= color_setting(
                    ui,
                    t,
                    &mut ColorAutomation { registry: &mut app.auto, prefix: "timeline.zoomBar.settings.audioIndicatorColor" },
                    ui.make_persistent_id("timeline-zoombar-minimap-audio-color"),
                    tl!("Audio Indicator Color:"),
                    &mut settings.audio_indicator_color,
                    "#2e7d32",
                );
                changed |= color_setting(
                    ui,
                    t,
                    &mut ColorAutomation { registry: &mut app.auto, prefix: "timeline.zoomBar.settings.offlineIndicatorColor" },
                    ui.make_persistent_id("timeline-zoombar-minimap-offline-color"),
                    tl!("Offline Indicator Color:"),
                    &mut settings.offline_indicator_color,
                    "#d32f2f",
                );
            });
        });
        app.auto.add("timeline.zoomBar.settings.minimapFlyout", panel.response.rect, "Minimap Settings");
        family_push(ui, panel.response.rect);
        ui.data_mut(|d| d.insert_temp(rect_key, panel.response.rect));
        ui.data_mut(|d| d.insert_temp(height_key, panel.response.rect.height()));
    }
    if whole != settings.show_whole_timeline {
        settings.show_whole_timeline = whole;
        changed = true;
    }
    if fill != settings.fill_clips {
        settings.fill_clips = fill;
        changed = true;
    }
    if pri != settings.prioritise_audio {
        settings.prioritise_audio = pri;
        changed = true;
    }
    if pos_ind != settings.show_position_indicator {
        settings.show_position_indicator = pos_ind;
        changed = true;
    }
    // A click anywhere outside the colour picker aborts its keep-alive grace at once -- including
    // a click on the settings menu or the flyout themselves.
    if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
        && !ui.data(|d| d.get_temp::<Rect>(color_rect_key())).unwrap_or(Rect::NOTHING).contains(pos)
    {
        ui.data_mut(|d| d.insert_temp(grace_key(), -1e9f64));
    }
    if changed {
        // the colour wheel and the number fields report every frame of a drag: remember the
        // change, but bump the (cache-invalidating) revision only once the gesture ends
        ui.data_mut(|d| d.insert_temp(egui::Id::unique("tl-zoombar-view-pending"), app.session.document_epoch));
    }
    flush_pending_zoombar_view_change(app, ui.ctx());
}

fn apply_max_zoom_choice(settings: &mut filmcraft_engine::project::TimelineUiSettings, choice: usize, presets: &[f64; 4]) {
    match choice {
        0..=3 => {
            settings.max_zoom_seconds = presets[choice];
            settings.max_zoom_custom = false;
        }
        _ => {
            // Custom again: restore the last custom value, never reset it to a default
            if !settings.max_zoom_custom {
                settings.max_zoom_seconds = if settings.max_zoom_last_custom.is_finite() { settings.max_zoom_last_custom.clamp(0.05, 3.0) } else { 0.4 };
            }
            settings.max_zoom_custom = true;
        }
    }
}

/// A pill on/off switch for the zoom-bar settings popup: red while off, the accent blue while on.
fn pill_switch(ui: &mut egui::Ui, r: Rect, on: bool, t: &Tokens, id: egui::Id, label: &str) -> egui::Response {
    const SWITCH_OFF: Color32 = Color32::from_rgb(0xf8, 0x7e, 0x7a);
    const KNOB: Color32 = Color32::from_gray(245);
    let resp = ui.interact(r.expand(3.0), id, Sense::click());
    let p = ui.painter();
    let rad = r.height() / 2.0;
    if on {
        p.rect_filled(r, rad, t.accent);
        p.circle_filled(pos2(r.max.x - rad, r.center().y), rad - 3.0, KNOB);
    } else {
        p.rect_filled(r, rad, SWITCH_OFF);
        p.circle_filled(pos2(r.min.x + rad, r.center().y), rad - 3.0, KNOB);
    }
    resp.on_hover_text(label)
}

fn should_close_zoombar_popup_on_escape(is_open: bool, escape_pressed: bool, capture_active: bool) -> bool {
    is_open && escape_pressed && !capture_active
}

/// One centered pill row of a slider settings menu: a label and its switch, over the row's full
/// width so the rows line up and nothing sits off-center in the popup.
fn settings_pill_row(ui: &mut egui::Ui, t: &Tokens, row_w: f32, label: &str, id: egui::Id, on: bool) -> egui::Response {
    let (row, _) = ui.allocate_exact_size(egui::vec2(row_w, 30.0), Sense::hover());
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    let sw_w = 34.0;
    let group_w = sw_w + 10.0 + galley.size().x;
    let x0 = row.min.x + ((row_w - group_w) / 2.0).max(0.0);
    let sw = Rect::from_min_size(pos2(x0, row.center().y - 8.0), egui::vec2(sw_w, 16.0));
    let c = pill_switch(ui, sw, on, t, id, label);
    ui.painter().galley(pos2(x0 + sw_w + 10.0, row.center().y - galley.size().y / 2.0), galley, t.text);
    c
}

/// The next key this frame, as the text a Controls box shows (for example "Ctrl + Shift + K").
/// Mouse buttons are not accepted as bindings because they would collide with Timeline gestures.
fn input_text(ui: &egui::Ui, pressed_only: bool) -> Option<String> {
    ui.input(|i| {
        let mut out = None;
        let mut events = i.events.clone();
        crate::panels::timeline::clipboard_events_as_keys(&mut events, i.modifiers, false);
        for e in &events {
            // The press OR the release: the shortcut matcher consumes presses out of the
            // frame's events (bound keys), but the release always survives.
            if let egui::Event::Key { key, modifiers, pressed, .. } = e
                && (!pressed_only || *pressed)
            {
                let mut s = String::new();
                if modifiers.ctrl || modifiers.command || modifiers.mac_cmd {
                    s.push_str("Ctrl + ");
                }
                if modifiers.shift {
                    s.push_str("Shift + ");
                }
                if modifiers.alt {
                    s.push_str("Alt + ");
                }
                s.push_str(&format!("{key:?}"));
                out = Some(s);
            }
        }
        out
    })
}

/// Let custom slider bindings reach their owning widget before a menu shortcut consumes them.
pub fn custom_control_shortcut_pending(app: &FilmcraftApp, ctx: &egui::Context) -> bool {
    let (events, modifiers) = ctx.input(|input| (input.events.clone(), input.modifiers));
    custom_control_shortcut_pending_for_events(app, ctx, &events, modifiers)
}

fn timeline_consumer_available(app: &FilmcraftApp) -> bool {
    !app.ui.fullscreen_program && app.ui.mode == crate::state::Mode::Edit && crate::dock::LayoutExt::is_visible(&app.ui.dock, crate::dock::PanelKind::Timeline)
}

pub fn custom_control_shortcut_pending_for_events(app: &FilmcraftApp, ctx: &egui::Context, events: &[egui::Event], modifiers: egui::Modifiers) -> bool {
    if controls_listening(app, ctx) || app.session.active_sequence().is_none() || !timeline_consumer_available(app) {
        return false;
    }
    let mut normalized = events.to_vec();
    crate::panels::timeline::clipboard_events_as_keys(&mut normalized, modifiers, false);
    let paste_press_will_be_recovered = !app.v_down
        && modifiers.command
        && events.iter().any(|event| matches!(event, egui::Event::Key { key: egui::Key::V, pressed: false, .. }))
        && !normalized.iter().any(|event| matches!(event, egui::Event::Key { key: egui::Key::V, pressed: true, .. }));
    if paste_press_will_be_recovered {
        normalized.push(egui::Event::Key {
            key: egui::Key::V,
            physical_key: Some(egui::Key::V),
            pressed: true,
            repeat: false,
            modifiers: modifiers | egui::Modifiers::COMMAND,
        });
    }
    let chord = normalized
        .iter()
        .filter_map(|event| {
            let egui::Event::Key { key, modifiers, pressed: true, .. } = event else { return None };
            let mut text = String::new();
            if modifiers.ctrl || modifiers.command || modifiers.mac_cmd {
                text.push_str("Ctrl + ");
            }
            if modifiers.shift {
                text.push_str("Shift + ");
            }
            if modifiers.alt {
                text.push_str("Alt + ");
            }
            text.push_str(&format!("{key:?}"));
            Some(text)
        })
        .next_back();
    let Some(chord) = chord else { return false };
    let chord_allowed = chord.contains("Ctrl") || chord.contains("Alt") || !ctx.text_edit_focused();
    if !chord_allowed {
        return false;
    }
    let settings = &app.session.state.timeline_ui;
    let matches = |binding: &str| !binding.is_empty() && !binding.ends_with("Click") && binding == chord;
    let pointer = ctx.input(|input| input.pointer.hover_pos());
    let zoom_over_bar = pointer.is_some_and(|pos| automation_contains(app, "timeline.zoomBar.track", pos));
    let zoom_drag = matches!(app.tl.drag, Some(super::Drag::ZoomBar { .. }));
    let zoom_action = !settings.zoom_bar_hidden
        && [settings.control_center_on_playhead.as_str(), settings.control_explode_slider.as_str()].into_iter().any(matches)
        || !settings.zoom_bar_hidden && zoom_drag && matches(&settings.control_alternate_drag_speed)
        || !settings.zoom_bar_hidden && zoom_over_bar && matches(&settings.control_settings);
    let track_over_bar =
        pointer.is_some_and(|pos| ["timeline.trackSlider.video.bar", "timeline.trackSlider.audio.bar"].into_iter().any(|id| automation_contains(app, id, pos)));
    let track_action = !settings.track_slider_hidden
        && ((track_over_bar
            && [settings.track_control_copy_slider.as_str(), settings.track_control_minimum_height.as_str(), settings.track_control_settings.as_str()]
                .into_iter()
                .any(matches))
            || app.tl.track_slider_dragging && matches(&settings.track_control_alternate_drag_speed));
    zoom_action || track_action
}

fn automation_contains(app: &FilmcraftApp, id: &str, pos: Pos2) -> bool {
    app.auto.find(id).is_some_and(|element| {
        let [x, y, width, height] = element.rect;
        Rect::from_min_size(pos2(x, y), vec2(width, height)).contains(pos)
    })
}

/// One Controls binding (0 = Center on Playhead, 1 = Explode Slider, 2 = Alternate Drag Speed,
/// 3 = Settings): empty means "the built-in gesture".
fn control_binding(app: &mut FilmcraftApp, slot: usize) -> &mut String {
    let s = &mut app.session.state.timeline_ui;
    match slot {
        0 => &mut s.control_center_on_playhead,
        1 => &mut s.control_explode_slider,
        2 => &mut s.control_alternate_drag_speed,
        3 => &mut s.control_settings,
        4 => &mut s.track_control_copy_slider,
        5 => &mut s.track_control_minimum_height,
        6 => &mut s.track_control_alternate_drag_speed,
        _ => &mut s.track_control_settings,
    }
}

/// Whether a captured key chord is currently held. The saved display form is generated from
/// egui's `Key` debug name, so comparing the held key's name preserves the same encoding.
fn binding_held(ui: &egui::Ui, binding: &str) -> bool {
    if binding.is_empty() {
        return false;
    }
    ui.input(|i| {
        i.keys_down
            .iter()
            .any(|key| binding_matches_held_key(binding, &format!("{key:?}"), i.modifiers.ctrl || i.modifiers.mac_cmd, i.modifiers.shift, i.modifiers.alt))
    })
}

fn binding_matches_held_key(binding: &str, key: &str, ctrl: bool, shift: bool, alt: bool) -> bool {
    let parts: Vec<_> = binding.split(" + ").collect();
    parts.last().copied() == Some(key) && parts.contains(&"Ctrl") == ctrl && parts.contains(&"Shift") == shift && parts.contains(&"Alt") == alt
}

/// The width the Controls flyout always has -- the main slider's -- in every menu.
const CONTROLS_FLYOUT_W: f32 = 257.4;

/// The Shift-drag rates of the track sliders: thumb points per pointer point for the height zoom,
/// content points for the scroll. Both are fixed constants times the Alternate Drag Speed setting
/// SQUARED -- never anything the track counts or the scroll range can skew, so the two sliders
/// move 1:1 with each other.
const SHIFT_THUMB_RATE: f64 = 0.06;
const SHIFT_CONTENT_RATE: f64 = 0.2;

/// The Alternate Drag Speed setting squared: 1x is the slow rate, Custom 10x a hundred times it.
fn shift_drag_rate(speed: f32) -> f64 {
    let s = f64::from(speed.max(0.01));
    s * s
}

/// Whether the minimap's clip indicators fit on screen: hidden below a quarter of the SCREEN's
/// width (the height never matters). A pure predicate, decided fresh every frame.
fn minimap_clips_fit(bar_w: f32, screen_w: f32) -> bool {
    bar_w >= 0.25 * screen_w.max(1.0)
}

/// Which side a flyout fits on: to the right of the menu when there is room there, otherwise to
/// the left. Computed from geometry alone, so the row's arrow points the right way on the very
/// frame the menu opens -- never a frame of one side and then the other.
fn flyout_to_the_right(ui: &egui::Ui, _row: Rect, content_right: f32, flyout_content_w: f32) -> bool {
    let margin = ui.spacing().menu_margin;
    let popup_right = content_right + f32::from(margin.right);
    let flyout_w = flyout_content_w + 16.0 + f32::from(margin.left) + f32::from(margin.right);
    popup_right + 4.0 + flyout_w <= ui.ctx().content_rect().right()
}

/// The little arrow on a Controls / Minimap row: at the same distance from whichever wall the
/// flyout opens on -- 6 pt past the content edge on the right, the mirror of that distance on the
/// left -- pointing at the flyout. (Measured off the popup's content edge, never the row's own
/// width: the switch rows are narrower and used to sit almost against the left wall.)
fn controls_arrow(ui: &egui::Ui, row: Rect, menu_w: f32, content_right: f32, to_the_right: bool, color: Color32) {
    let x = if to_the_right { row.min.x + menu_w - 6.0 } else { content_right - menu_w + 6.0 };
    let glyph = if to_the_right { "\u{25b6}" } else { "\u{25c0}" };
    ui.painter().text(pos2(x, row.center().y), Align2::CENTER_CENTER, glyph, Tokens::ui(8.0), color);
}

/// Where a Controls box that is listening for a new input remembers itself.
pub(crate) fn controls_listen_key() -> egui::Id {
    egui::Id::unique("tl-controls-listening")
}

fn controls_listening_for_bar(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<Option<usize>>(controls_listen_key())).flatten().is_some()
}

/// Whether a Controls box is listening for a new shortcut. While it is, the rest of the
/// application must not hear the key: the shortcut matcher consumes presses from the frame's
/// events (which is also why the capture used to miss them), and a bound key would fire its
/// action in the background while the user is naming a new binding.
pub fn controls_listening(app: &FilmcraftApp, ctx: &egui::Context) -> bool {
    let timeline_available = timeline_consumer_available(app);
    let zoom_bar_menu_open = egui::Popup::is_id_open(ctx, egui::Id::unique("tl-zoombar").with("popup"));
    let custom_zoom_popup_open = ctx.data(|data| data.get_temp::<bool>(zoombar_popup_open_key())).unwrap_or(false);
    let session_open = app.tl.zoombar_settings_pos.is_some() || app.tl.track_slider_settings_pos.is_some() || zoom_bar_menu_open || custom_zoom_popup_open;
    ctx.data_mut(|d| {
        let listening = d.get_temp::<Option<usize>>(controls_listen_key()).flatten().is_some();
        if listening && (!session_open || !timeline_available) {
            d.insert_temp(controls_listen_key(), None::<usize>);
            false
        } else {
            listening
        }
    })
}

/// While a Controls box listens, the next input becomes its binding (Escape cancels). Runs from
/// the settings menus every frame -- not only while the flyout draws -- so a flicker of the flyout
/// can never eat the input.
fn capture_controls(app: &mut FilmcraftApp, ui: &egui::Ui) {
    let key = controls_listen_key();
    let Some(slot) = ui.ctx().data(|d| d.get_temp::<Option<usize>>(key)).flatten() else {
        return;
    };
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.ctx().data_mut(|d| d.insert_temp(key, None::<usize>));
        return;
    }
    if let Some(text) = input_text(ui, false) {
        *control_binding(app, slot) = text.clone();
        app.session.project_view_changed();
        suppress_captured_chord(ui, &text);
        ui.ctx().data_mut(|d| d.insert_temp(key, None::<usize>));
    }
}

fn suppress_captured_chord(ui: &egui::Ui, chord: &str) {
    // The owning slider is drawn later in this same pass. Remove the key event after capture so
    // the newly bound Copy/Minimum action cannot also run from the chord that created the binding.
    ui.input_mut(|input| {
        let chord_of = |key: egui::Key, modifiers: egui::Modifiers| {
            let mut text = String::new();
            if modifiers.ctrl || modifiers.command || modifiers.mac_cmd {
                text.push_str("Ctrl + ");
            }
            if modifiers.shift {
                text.push_str("Shift + ");
            }
            if modifiers.alt {
                text.push_str("Alt + ");
            }
            text.push_str(&format!("{key:?}"));
            text
        };
        let modifiers = input.modifiers;
        input.events.retain(|event| {
            if let egui::Event::Key { key, modifiers, .. } = event {
                return chord_of(*key, *modifiers) != chord;
            }
            if matches!(event, egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) | egui::Event::PasteImage(_)) {
                let mut normalized = vec![event.clone()];
                crate::panels::timeline::clipboard_events_as_keys(&mut normalized, modifiers, false);
                return !normalized.iter().any(|candidate| matches!(candidate, egui::Event::Key { key, modifiers, .. } if chord_of(*key, *modifiers) == chord));
            }
            true
        });
    });
}

/// The hover flyout beside a settings row (the Minimap Settings' sibling): the very same shape as
/// the minimap's own flyout -- placed beside the menu (to the right when the screen has room for
/// it there, otherwise to the left), bottom-aligned with the row's bottom edge, always fully
/// opaque -- and the very same keep-alive: plain geometry against the raw pointer position (the
/// row, the ring of gaps around the menu, the flyout itself, the settings' own popups) plus the
/// colour pickers' 3 s grace. egui's `rect_contains_pointer` is layer-bound and reports false for
/// the gap strips and for the flyout's own Area, which is exactly why a flyout used to vanish the
/// moment the pointer left its row. Only `trigger` (a real hover of the row, after the pointer has
/// moved) OPENS it; everything else merely keeps it open. No settings in it yet.
#[allow(clippy::too_many_arguments)]
fn hover_flyout(
    app: &mut FilmcraftApp,
    ui: &mut egui::Ui,
    flyout_id: egui::Id,
    row: Rect,
    content_right: f32,
    content_w: f32,
    open_now: bool,
    auto_id: &str,
    auto_label: &str,
    items: &[(&str, String, usize)],
) {
    remember_flyout_id(ui.ctx(), flyout_id);
    let rect_key = flyout_id.with("rect");
    let height_key = flyout_id.with("height");
    let stamp_key = flyout_id.with("stamp");
    let margin = ui.spacing().menu_margin;
    let popup_right = content_right + f32::from(margin.right);
    let popup_left = row.min.x - f32::from(margin.left);
    let flyout_w = content_w + 16.0 + f32::from(margin.left) + f32::from(margin.right);
    let screen = ui.ctx().content_rect();
    let gap = 4.0;
    let to_the_right = flyout_to_the_right(ui, row, content_right, content_w);
    let (x, to_the_right) = if to_the_right { (popup_right + gap, true) } else { ((popup_left - gap - flyout_w).max(screen.left()), false) };
    let popup_bottom = row.max.y + f32::from(margin.bottom);
    let flyout_h = ui.data(|d| d.get_temp::<f32>(height_key)).unwrap_or(110.0);
    // the flyout centers on the setting row (its own middle at the row's middle), never past the
    // screen's edges
    let y = (row.center().y - flyout_h / 2.0).clamp(screen.top(), (screen.bottom() - flyout_h).max(screen.top()));
    let flyout_rect = Rect::from_min_size(pos2(x, y), egui::vec2(flyout_w, flyout_h));
    // the keep-alive frame around the menu: the pointer may travel anywhere on the menu or in a
    // 24 pt ring of gaps around it (and across the gap strip to the flyout) without losing it
    let menu = Rect::from_min_max(pos2(popup_left, row.min.y - 30.0 - f32::from(margin.top)), pos2(popup_right, popup_bottom));
    let (gx0, gx1) = if to_the_right { (popup_right, x) } else { (x + flyout_w, popup_left) };
    let side = Rect::from_min_max(pos2(gx0.min(gx1), y), pos2(gx0.max(gx1), popup_bottom.max(y + flyout_h)));
    let under = Rect::from_min_max(pos2(popup_left, popup_bottom), pos2(popup_right, popup_bottom + 24.0));
    let around = Rect::from_min_max(pos2(menu.min.x - 24.0, menu.min.y - 24.0), pos2(menu.max.x + 24.0, menu.max.y + 24.0));
    let pointer = ui.input(|i| i.pointer.hover_pos().or(i.pointer.interact_pos()));
    let over = |r: Rect| pointer.is_some_and(|p| r.contains(p));
    let prev_flyout = ui.data(|d| d.get_temp::<Rect>(rect_key)).unwrap_or(flyout_rect);
    let now = ui.input(|i| i.time);
    // working in one of the settings' own popups (or the three seconds after it) keeps the flyout
    // alive no matter where the pointer goes
    let keep = ui.data(|d| d.get_temp::<f64>(grace_key())).unwrap_or(-1e9);
    let row_hot = over(row) && open_now;
    let keep_alive = row_hot
        || over(menu)
        || over(side)
        || over(under)
        || over(around)
        || over(prev_flyout)
        || over(flyout_rect)
        || pointer.is_some_and(|p| family_contains_prev(ui, p))
        || now - keep < 3.0;
    // the ROW opens it -- and only on a real hover of it: the menu appears under the cursor, often
    // right on this row, so the pointer has to move first
    let opened_at = ui.data(|d| d.get_temp::<Pos2>(open_pointer_key())).unwrap_or(pos2(-1e9, -1e9));
    let moved = pointer.is_some_and(|p| (p - opened_at).length() > 6.0);
    let trigger = row_hot && moved;
    let was_open = ui.data(|d| d.get_temp::<bool>(flyout_id.with("open"))).unwrap_or(false);
    let stamp = ui.data(|d| d.get_temp::<f64>(stamp_key)).unwrap_or(-1e9);
    let open = open_now && (trigger || (was_open && (keep_alive || now - stamp < 0.3)));
    if trigger || keep_alive {
        ui.data_mut(|d| d.insert_temp(stamp_key, now));
    }
    ui.data_mut(|d| d.insert_temp(flyout_id.with("open"), open));
    if open {
        let panel = egui::Area::new(flyout_id).order(egui::Order::Foreground).fixed_pos(pos2(x, y)).fade_in(false).interactable(true).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(content_w + 16.0);
                ui.set_max_width(content_w + 16.0);
                // the control list: an action and the input that does it (the default gesture in
                // the dark box until the user rebinds it). Clicking a box listens for the next
                // input -- a key chord -- which becomes the new way to do it;
                // the circular arrow at the row's end restores the default. (The listening itself
                // happens in [`capture_controls`], from the menus, every frame.)
                let listening = ui.ctx().data(|d| d.get_temp::<Option<usize>>(controls_listen_key())).flatten();
                for (idx, (name, default, slot)) in items.iter().enumerate() {
                    // the row spans the flyout's content width, exactly as the Minimap colour
                    // rows do, so the reset arrow lands at their distance from the right wall
                    let row_w = ui.available_width().max(170.0);
                    let (irow, _) = ui.allocate_exact_size(egui::vec2(row_w, 28.0), Sense::hover());
                    let n_galley = ui.painter().layout_no_wrap((*name).to_string(), Tokens::ui(11.5), app.tokens.text);
                    let bound = control_binding(app, *slot).clone();
                    let shown = if listening == Some(*slot) {
                        tl!("Enter Shortcut").to_string()
                    } else if bound.is_empty() {
                        default.clone()
                    } else {
                        bound.clone()
                    };
                    let a_galley = ui.painter().layout_no_wrap(shown.clone(), Tokens::ui(11.5), egui::Color32::WHITE);
                    let gap = 6.0;
                    let box_w = a_galley.size().x.max(52.0) + 14.0;
                    let group_w = n_galley.size().x + gap + box_w;
                    let ix0 = irow.center().x - group_w / 2.0;
                    let ibox = Rect::from_min_size(pos2(ix0 + n_galley.size().x + gap, irow.center().y - 10.0), egui::vec2(box_w, 20.0));
                    let bresp = ui.interact(ibox, flyout_id.with(("box", idx)), Sense::click());
                    if bresp.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if bresp.clicked() {
                        ui.ctx().data_mut(|d| d.insert_temp(controls_listen_key(), Some(*slot)));
                    }
                    app.auto.add(&format!("{auto_id}.box{idx}"), ibox, name);
                    ui.painter().rect_filled(ibox, 3.0, egui::Color32::from_gray(16));
                    ui.painter().rect_stroke(ibox, 3.0, egui::Stroke::new(1.0, egui::Color32::from_gray(64)), egui::StrokeKind::Inside);
                    ui.painter().text(ibox.center(), Align2::CENTER_CENTER, shown, Tokens::ui(11.0), egui::Color32::WHITE);
                    ui.painter().galley(pos2(ix0, irow.center().y - n_galley.size().y / 2.0), n_galley, app.tokens.text);
                    // the circular reset arrow, the Effect Controls / Minimap colour rows' one
                    let rr = Rect::from_center_size(pos2(irow.max.x - 14.0, irow.center().y), vec2(16.0, 16.0));
                    let rresp = ui.interact(rr, flyout_id.with(("reset", idx)), Sense::click()).on_hover_text(tl!("Reset"));
                    app.auto.add(&format!("{auto_id}.reset{idx}"), rr, tl!("Reset"));
                    icons::paint(ui.painter(), rr.shrink(2.0), Icon::Reset, if rresp.hovered() { app.tokens.text } else { app.tokens.text_dim });
                    if rresp.clicked() {
                        *control_binding(app, *slot) = String::new();
                        app.session.project_view_changed();
                    }
                }
            });
        });
        app.auto.add(auto_id, panel.response.rect, auto_label);
        family_push(ui, panel.response.rect);
        ui.data_mut(|d| d.insert_temp(rect_key, panel.response.rect));
        ui.data_mut(|d| d.insert_temp(height_key, panel.response.rect.height()));
    }
}

/// The zoom-bar settings' own little popups (the Minimap Settings flyout, the colour pickers):
/// their rects, collected fresh every frame while [`zoombar_settings`] draws, so a "click outside"
/// check can tell a click on one of them from a click away from the settings.
fn family_reset(ui: &egui::Ui) {
    // last frame's rects become "prev": the flyout's keep-alive must recognise this frame's
    // children (a colour picker under it) before they are drawn again
    let cur = ui.data_mut(|d| d.get_temp::<Vec<Rect>>(egui::Id::unique("tl-zoombar-family")).unwrap_or_default());
    ui.data_mut(|d| {
        d.insert_temp(egui::Id::unique("tl-zoombar-family-prev"), cur);
        d.insert_temp(egui::Id::unique("tl-zoombar-family"), Vec::<Rect>::new());
    });
}

fn family_push(ui: &egui::Ui, rect: Rect) {
    ui.data_mut(|d| {
        let mut v = d.get_temp::<Vec<Rect>>(egui::Id::unique("tl-zoombar-family")).unwrap_or_default();
        v.push(rect);
        d.insert_temp(egui::Id::unique("tl-zoombar-family"), v);
    });
}

fn family_contains(ui: &egui::Ui, pos: Pos2) -> bool {
    ui.data(|d| d.get_temp::<Vec<Rect>>(egui::Id::unique("tl-zoombar-family")).unwrap_or_default()).iter().any(|r| r.contains(pos))
}

fn family_contains_prev(ui: &egui::Ui, pos: Pos2) -> bool {
    ui.data(|d| d.get_temp::<Vec<Rect>>(egui::Id::unique("tl-zoombar-family-prev")).unwrap_or_default()).iter().any(|r| r.contains(pos))
}

/// Shared one-per-popup state of the settings' little popups: which colour picker is open, where
/// it sits, and the time its keep-alive grace runs from.
fn color_open_key() -> egui::Id {
    egui::Id::unique("tl-zoombar-color-open")
}

fn color_rect_key() -> egui::Id {
    egui::Id::unique("tl-zoombar-color-rect")
}

fn grace_key() -> egui::Id {
    egui::Id::unique("tl-zoombar-grace")
}

/// Where the pointer was when the settings menu opened: the flyout only opens on a real hover of
/// its row, never just because the menu appeared under the cursor on that row.
fn open_pointer_key() -> egui::Id {
    egui::Id::unique("tl-zoombar-open-pointer")
}

/// Forget every transient of the settings popups (the Minimap flyout, the colour pickers and
/// their keep-alives): the next time the menu opens it is in its default state, never with a
/// picker or flyout left over from the last time.
pub(super) fn zoombar_state_reset(ctx: &egui::Context) {
    let pointer = ctx.input(|i| i.pointer.hover_pos()).unwrap_or(pos2(-1e9, -1e9));
    let flyout_ids = ctx.data(|data| data.get_temp::<Vec<egui::Id>>(flyout_id_registry_key()).unwrap_or_default());
    ctx.data_mut(|d| {
        d.insert_temp(color_open_key(), None::<egui::Id>);
        d.insert_temp(controls_listen_key(), None::<usize>);
        d.insert_temp(color_rect_key(), Rect::NOTHING);
        d.insert_temp(grace_key(), -1e9f64);
        d.insert_temp(open_pointer_key(), pointer);
        d.insert_temp(egui::Id::unique("tl-zoombar-family"), Vec::<Rect>::new());
        d.insert_temp(egui::Id::unique("tl-zoombar-family-prev"), Vec::<Rect>::new());
        for id in flyout_ids {
            d.insert_temp(id.with("rect"), Rect::NOTHING);
            d.insert_temp(id.with("stamp"), -1e9f64);
            d.insert_temp(id.with("height"), 110.0f32);
            d.insert_temp(id.with("open"), false);
        }
    });
}

fn flyout_id_registry_key() -> egui::Id {
    egui::Id::unique("timeline-zoombar-flyout-id-registry")
}

fn remember_flyout_id(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|data| {
        let mut ids = data.get_temp::<Vec<egui::Id>>(flyout_id_registry_key()).unwrap_or_default();
        if !ids.contains(&id) {
            ids.push(id);
            data.insert_temp(flyout_id_registry_key(), ids);
        }
    });
}

/// Parse "#RRGGBB" or "#RGB" into a colour; anything else gives the fallback (hostile project and
/// control-channel text included).
fn parse_hex_color(s: &str, fallback: Color32) -> Color32 {
    fn parse(s: &str) -> Option<Color32> {
        let t = s.trim().strip_prefix('#').unwrap_or(s.trim()).as_bytes();
        let digit = |c: u8| char::from(c).to_digit(16);
        let mut v = [0u8; 3];
        if t.len() == 6 {
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = ((digit(t[i * 2])? * 16 + digit(t[i * 2 + 1])?) & 0xff) as u8;
            }
        } else if t.len() == 3 {
            for (i, slot) in v.iter_mut().enumerate() {
                let d = digit(t[i])?;
                *slot = ((d * 17) & 0xff) as u8;
            }
        } else {
            return None;
        }
        Some(Color32::from_rgb(v[0], v[1], v[2]))
    }
    parse(s).unwrap_or(fallback)
}

/// A colour as "#rrggbb", the shape the setting stores.
fn hex_color(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

/// HSV (hue 0-360, saturation and value 0-1) to a colour, for the picker's wheel.
fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let h = (((h % 360.0) + 360.0) % 360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

/// A colour back to hue / saturation, for the picker's wheel (the wheel has no brightness).
fn color_to_hsv(c: Color32) -> (f32, f32) {
    let r = f32::from(c.r()) / 255.0;
    let g = f32::from(c.g()) / 255.0;
    let b = f32::from(c.b()) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (if h < 0.0 { h + 360.0 } else { h }, if max <= 0.0 { 0.0 } else { d / max })
}

/// Any hex digits to the closest "#rrggbb": three digits expand the CSS way, longer ones are cut
/// to six, shorter ones are filled out to six. Lowercase, always exactly six digits -- and never
/// panics, not even on no digits at all (clearing the hex field once crashed here).
fn closest_six(digits: &str) -> String {
    // Only six digits can affect the resulting colour; never copy an unbounded paste.
    let d: Vec<char> = digits.chars().take(6).collect();
    let six: String = if d.is_empty() {
        "000000".to_string()
    } else if d.len() == 3 {
        d.iter().flat_map(|c| [*c, *c]).collect()
    } else if d.len() > 6 {
        d[..6].iter().collect()
    } else {
        (0..6).map(|i| d[i % d.len()]).collect()
    };
    format!("#{six}").to_lowercase()
}

/// A colour setting row: the label, and a dark swatch (a lighter outline) showing the colour's hex
/// code in white. Clicking the swatch (a pointing hand) opens the colour picker -- a hue /
/// saturation wheel, a brightness slider and a hex field, everything live. ONE call draws the row
/// and runs its picker; copy that one line to add another colour setting anywhere.
struct ColorAutomation<'a> {
    registry: &'a mut crate::automation::Registry,
    prefix: &'a str,
}

impl ColorAutomation<'_> {
    fn add(&mut self, suffix: &str, response: &egui::Response, label: &str) {
        self.registry.add_response(&format!("{}.{}", self.prefix, suffix), response, label);
    }
}

fn color_setting(ui: &mut egui::Ui, t: &Tokens, automation: &mut ColorAutomation<'_>, id: egui::Id, label: &str, color: &mut String, default: &str) -> bool {
    let mut changed = false;
    let row_w = ui.available_width().max(170.0);
    let (row, _) = ui.allocate_exact_size(egui::vec2(row_w, 30.0), Sense::hover());
    let base = parse_hex_color(color, Color32::from_rgb(0x58, 0x95, 0xec));
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::ui(11.5), t.text);
    // the row is a centered group like the pill rows: label, a small gap, the hex box
    let box_w = 72.0;
    let gap = 6.0;
    let group_w = galley.size().x + gap + box_w;
    let x0 = row.center().x - group_w / 2.0;
    let swatch = Rect::from_min_size(pos2(x0 + galley.size().x + gap, row.center().y - 11.0), egui::vec2(box_w, 22.0));
    let resp = ui.interact(swatch, id, Sense::click());
    automation.add("swatch", &resp, label);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(swatch, 3.0, Color32::from_gray(16));
    p.rect_stroke(swatch, 3.0, Stroke::new(1.0, Color32::from_gray(64)), StrokeKind::Inside);
    p.text(swatch.center(), Align2::CENTER_CENTER, color.as_str(), Tokens::ui(12.5), Color32::WHITE);
    ui.painter().galley(pos2(x0, row.center().y - galley.size().y / 2.0), galley, t.text);
    // the reset button at the row's right end, the same as Effect Controls' parameter reset
    let rr = Rect::from_center_size(pos2(row.max.x - 14.0, row.center().y), vec2(16.0, 16.0));
    let rresp = ui.interact(rr, id.with("reset"), Sense::click()).on_hover_text(tl!("Reset"));
    automation.add("reset", &rresp, &format!("Reset {label}"));
    icons::paint(ui.painter(), rr.shrink(2.0), Icon::Reset, if rresp.hovered() { t.text } else { t.text_dim });
    if rresp.clicked() {
        *color = default.to_string();
        changed = true;
        ui.data_mut(|d| {
            d.insert_temp(id.with("hex"), color.clone());
            d.insert_temp(id.with("hex-observed-color"), color.clone());
        });
    }
    // one colour picker open at a time, remembered under a shared key so opening the menu afresh
    // can clear it
    let open_now = ui.data(|d| d.get_temp::<Option<egui::Id>>(color_open_key())).flatten() == Some(id);
    if resp.clicked() {
        ui.data_mut(|d| {
            d.insert_temp(color_open_key(), if open_now { None } else { Some(id) });
            if open_now {
                d.remove::<String>(id.with("hex"));
                d.remove::<String>(id.with("hex-observed-color"));
            } else {
                d.insert_temp(id.with("hex"), color.clone());
                d.insert_temp(id.with("hex-observed-color"), color.clone());
            }
        });
    }
    if open_now {
        let mut hex_focus = false;
        let panel = egui::Area::new(id.with("wheel"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(swatch.min.x - 30.0, swatch.max.y + 4.0))
            .fade_in(false)
            .interactable(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // the wheel: hue around the ring, saturation towards the edge. One seamless
                    // mesh, so it is just colours -- no seams between the parts.
                    let (slot, resp) = ui.allocate_exact_size(egui::vec2(132.0, 132.0), Sense::click_and_drag());
                    automation.add("wheel", &resp, &format!("{label} color wheel"));
                    let c = slot.center();
                    let outer = 62.0;
                    let mut mesh = egui::Mesh::default();
                    const ANGLES: usize = 48;
                    const RINGS: usize = 3;
                    for ring in 0..=RINGS {
                        for seg in 0..=ANGLES {
                            let a = seg as f32 / ANGLES as f32 * std::f32::consts::TAU;
                            let r = outer * ring as f32 / RINGS as f32;
                            let col = hsv(seg as f32 / ANGLES as f32 * 360.0, ring as f32 / RINGS as f32, 1.0);
                            mesh.colored_vertex(c + vec2(a.cos(), a.sin()) * r, col);
                        }
                    }
                    for ring in 0..RINGS {
                        for seg in 0..ANGLES {
                            let i = (ring * (ANGLES + 1) + seg) as u32;
                            let n = ANGLES as u32 + 1;
                            mesh.add_triangle(i, i + 1, i + n);
                            mesh.add_triangle(i + 1, i + n + 1, i + n);
                        }
                    }
                    ui.painter().add(egui::Shape::mesh(mesh));
                    let (h, s) = color_to_hsv(base);
                    let at = c + vec2(h.to_radians().cos(), h.to_radians().sin()) * (s * outer);
                    ui.painter().circle_stroke(at, 4.0, Stroke::new(1.5, Color32::WHITE));
                    if (resp.clicked() || resp.dragged())
                        && let Some(pos) = resp.interact_pointer_pos()
                    {
                        let d = pos - c;
                        let nh = (d.y.atan2(d.x).to_degrees() + 360.0) % 360.0;
                        let ns = (d.length() / outer).clamp(0.0, 1.0);
                        *color = hex_color(hsv(nh, ns, 1.0));
                        changed = true;
                    }
                    // the hex field, centered under the wheel: six digits only -- longer pastes
                    // are cut to six, shorter ones filled out, both to the closest colour
                    let mut buf = ui.data(|d| d.get_temp::<String>(id.with("hex"))).unwrap_or_else(|| color.clone());
                    let mut last_color = ui.data(|d| d.get_temp::<String>(id.with("hex-observed-color"))).unwrap_or_else(|| color.clone());
                    if changed {
                        // the wheel or Reset changed the colour outside the field: the stored
                        // text must follow it, or the field shows the last typed value again
                        buf = color.clone();
                        last_color = color.clone();
                        ui.data_mut(|d| {
                            d.insert_temp(id.with("hex"), buf.clone());
                            d.insert_temp(id.with("hex-observed-color"), last_color.clone());
                        });
                    }
                    // the hex field, dead center under the wheel (placed at an exact rect: layout
                    // alignment refused to centre it)
                    let (hrow, _) = ui.allocate_exact_size(egui::vec2(132.0, 26.0), Sense::hover());
                    let field = Rect::from_min_size(pos2(hrow.center().x - 46.0, hrow.center().y - 11.0), egui::vec2(92.0, 22.0));
                    let edit = ui.put(field, egui::TextEdit::singleline(&mut buf).char_limit(6).align(egui::Align2::CENTER_CENTER));
                    automation.add("hex", &edit, &format!("{label} hex value"));
                    hex_focus = edit.has_focus();
                    if edit.changed() {
                        let digits: String = buf.chars().filter(|c| c.is_ascii_hexdigit()).take(6).collect();
                        buf = format!("#{digits}");
                        ui.data_mut(|d| d.insert_temp(id.with("hex"), buf.clone()));
                        if !digits.is_empty() {
                            *color = closest_six(&digits);
                            changed = true;
                        }
                    }
                    sync_external_color_buffer(&mut buf, &mut last_color, color, edit.has_focus());
                    ui.data_mut(|d| {
                        d.insert_temp(id.with("hex"), buf.clone());
                        d.insert_temp(id.with("hex-observed-color"), last_color.clone());
                    });
                });
            });
        family_push(ui, panel.response.rect);
        ui.data_mut(|d| d.insert_temp(color_rect_key(), panel.response.rect));
        // Working in the picker keeps every menu alive for three more seconds after it stops
        // (wheel drag, the hex field's focus, or simply pointing at the picker).
        let now = ui.input(|i| i.time);
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let touching = resp.dragged() || hex_focus || pointer.is_some_and(|p| panel.response.rect.contains(p));
        if touching {
            ui.data_mut(|d| d.insert_temp(grace_key(), now));
        }
        // a click outside the picker and its swatch closes it
        if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
            && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
            && !panel.response.rect.contains(pos)
            && !swatch.contains(pos)
        {
            ui.data_mut(|d| {
                d.insert_temp(color_open_key(), None::<egui::Id>);
                d.remove::<String>(id.with("hex"));
                d.remove::<String>(id.with("hex-observed-color"));
            });
        }
    }
    changed
}

/// The zoom-bar settings popup opened from the Timeline's own menu ("Timeline Slider Settings",
/// shown only while the bar is hidden): the exact same settings, the same way, as right-clicking
/// the bar itself.
pub(super) fn zoombar_settings_popup(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let Some(pos) = app.tl.zoombar_settings_pos else {
        ui.ctx().data_mut(|data| data.remove::<bool>(zoombar_settings_top_anchor_key()));
        return;
    };
    let screen = ui.ctx().content_rect();
    let content_height = ui.ctx().data(|data| data.get_temp::<f32>(zoombar_popup_content_height_key())).unwrap_or(430.0);
    let needs_scroll = content_height + 16.0 > screen.height();
    let top_anchor_key = zoombar_settings_top_anchor_key();
    let anchored_at_top = ui.ctx().data(|data| data.get_temp::<bool>(top_anchor_key)).unwrap_or(false) || needs_scroll;
    ui.ctx().data_mut(|data| data.insert_temp(top_anchor_key, anchored_at_top));
    // Once a resize forces the popup to the viewport top, keep that anchor until it closes. This
    // lets the visible area grow continuously with the window instead of snapping back to the
    // menu-button position when the scroll bar disappears.
    let pos = if anchored_at_top {
        pos2(pos.x, screen.top() + 8.0)
    } else if pos.y + content_height + 16.0 > screen.bottom() {
        pos2(pos.x, (pos.y - content_height - 16.0).max(screen.top() + 8.0))
    } else {
        pos
    };
    let panel = egui::Area::new(egui::Id::unique("tl-zoombar-settings-from-menu")).order(egui::Order::Foreground).fixed_pos(pos).interactable(true).show(
        ui.ctx(),
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                zoombar_settings(app, ui);
            });
        },
    );
    if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
        && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        && !panel.response.rect.contains(p)
        && !family_contains(ui, p)
    {
        app.tl.zoombar_settings_pos = None;
        ui.ctx().data_mut(|data| data.remove::<bool>(top_anchor_key));
    }
}

fn zoombar_settings_top_anchor_key() -> egui::Id {
    egui::Id::unique("tl-zoombar-settings-top-anchor")
}

pub(crate) fn flush_pending_zoombar_view_change(app: &mut FilmcraftApp, ctx: &egui::Context) {
    flush_pending_zoombar_view_change_with_force(app, ctx, false);
}

fn flush_pending_zoombar_view_change_with_force(app: &mut FilmcraftApp, ctx: &egui::Context, force: bool) {
    let id = egui::Id::unique("tl-zoombar-view-pending");
    if let Some(document_epoch) = ctx.data(|data| data.get_temp::<u64>(id)) {
        if document_epoch != app.session.document_epoch {
            ctx.data_mut(|data| data.remove_temp::<u64>(id));
        } else if force || !ctx.input(|input| input.pointer.any_down()) {
            ctx.data_mut(|data| data.remove_temp::<u64>(id));
            app.session.project_view_changed();
        }
    }
}

pub(crate) fn flush_pending_view_changes_for_close(app: &mut FilmcraftApp, ctx: &egui::Context) {
    flush_pending_zoombar_view_change_with_force(app, ctx, true);
    flush_track_slider_view_change_with_force(app, ctx, true);
}

/// The value to fall back to when Custom is picked again and no custom value was stored yet.
fn last_custom_value(stored: f64, max: f64) -> f64 {
    if stored.is_finite() { stored.clamp(0.1, max) } else { 15.0 }
}

/// A centered section heading for the settings popup (wrapped, so it stays inside a narrow menu).
fn settings_heading(ui: &mut egui::Ui, t: &Tokens, label: &str) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), Sense::hover());
    let galley = ui.painter().layout(label.to_string(), Tokens::ui(11.5), t.text, r.width().max(40.0));
    ui.painter().galley(pos2(r.center().x - galley.size().x / 2.0, r.center().y - galley.size().y / 2.0), galley, t.text);
}

fn tick_slider(ui: &mut egui::Ui, value: &mut usize, labels: &[&str], t: &Tokens) -> (egui::Response, bool) {
    let row_width = ui.available_width().max(100.0);
    let (row, response) = ui.allocate_exact_size(egui::vec2(row_width, 46.0), Sense::click_and_drag());
    // keep the widest end label inside the menu
    let margin = (row_width * 0.09).clamp(18.0, 24.0);
    let left = row.left() + margin;
    let right = row.right() - margin;
    let y = row.top() + 16.0;
    let steps = (labels.len() - 1).max(1) as f32;
    let x_at = |i: usize| left + (right - left) * i as f32 / steps;
    let before = *value;
    if (response.clicked() || response.dragged())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let frac = ((pos.x - left) / (right - left).max(1.0)).clamp(0.0, 1.0);
        *value = (frac * steps).round() as usize;
    }
    let p = ui.painter();
    p.line_segment([pos2(left, y), pos2(right, y)], Stroke::new(3.5, t.separator));
    p.line_segment([pos2(left, y), pos2(x_at(*value), y)], Stroke::new(3.5, t.accent));
    for (i, label) in labels.iter().enumerate() {
        p.line_segment([pos2(x_at(i), y - 6.0), pos2(x_at(i), y + 6.0)], Stroke::new(1.0, t.icon_active));
        p.text(pos2(x_at(i), y + 9.0), Align2::CENTER_TOP, *label, Tokens::ui(10.0), t.text_dim);
    }
    p.circle_filled(pos2(x_at(*value), y), 7.0, t.accent);
    (response, before != *value)
}

/// The track slider's thumb length for a track height: the same kind of zoom meter as the zoom
/// bar's thumb -- the shortest tracks fill the bar, the tallest leave the [`MIN_THUMB_W`]
/// minimum, log-smooth in between. A bijection, so a drag can never explode or compound.
fn track_slider_click_count(double_clicked: bool, copy_binding: bool, minimum_binding: bool, shift: bool, command: bool) -> u8 {
    if copy_binding {
        1
    } else if double_clicked || minimum_binding {
        if minimum_binding {
            2
        } else if shift && command {
            3
        } else if shift {
            2
        } else {
            1
        }
    } else {
        0
    }
}

fn track_thumb(track_h: f32, bar_h: f32) -> f32 {
    if !bar_h.is_finite() || bar_h <= 0.0 {
        return 0.0;
    }
    let track_h = if track_h.is_finite() { track_h.clamp(super::MIN_TRACK_HEIGHT, super::MAX_TRACK_HEIGHT) } else { super::MAX_TRACK_HEIGHT };
    let f = 1.0 - (track_h / super::MIN_TRACK_HEIGHT).log10() / (super::MAX_TRACK_HEIGHT / super::MIN_TRACK_HEIGHT).log10();
    let min_thumb = MIN_THUMB_W.min(bar_h);
    min_thumb + f.clamp(0.0, 1.0) * (bar_h - min_thumb)
}

/// Its inverse: the track height a thumb `len` points long stands for.
fn track_height_of(len: f32, bar_h: f32) -> f32 {
    if !bar_h.is_finite() || bar_h <= MIN_THUMB_W {
        return super::MAX_TRACK_HEIGHT;
    }
    let len = if len.is_finite() { len.clamp(MIN_THUMB_W, bar_h) } else { MIN_THUMB_W };
    let f = (len - MIN_THUMB_W) / (bar_h - MIN_THUMB_W);
    super::MIN_TRACK_HEIGHT * (super::MAX_TRACK_HEIGHT / super::MIN_TRACK_HEIGHT).powf(1.0 - f)
}

fn centered_track_scroll(scroll: f32, window_h: f32, old_track_h: f32, new_track_h: f32, max_scroll: f32) -> f32 {
    let max_scroll = if max_scroll.is_finite() { max_scroll.max(0.0) } else { 0.0 };
    if !scroll.is_finite() || !window_h.is_finite() || !old_track_h.is_finite() || old_track_h <= 0.0 || !new_track_h.is_finite() {
        return 0.0f32.clamp(0.0, max_scroll);
    }
    let center = (scroll + window_h / 2.0) * (new_track_h / old_track_h) - window_h / 2.0;
    if center.is_finite() { center.clamp(0.0, max_scroll) } else { 0.0f32.clamp(0.0, max_scroll) }
}

/// How far a Shift drag on the zoom bar moves the view per point of pointer motion, as a fraction
/// of an ordinary 1:1 drag. Tuned by feel -- log-interpolated between fixed points: one frame of
/// footage in view -> 0.00025 (per-pixel nudges at the tightest zoom), one second -> 0.00575,
/// 10 seconds -> 1/50, one minute -> 0.115, and fitting the whole Timeline -> 0.75. Narrow views get
/// frame-level precision, wide ones sweep quickly. One curve instead of the old
/// Auto-percent-times-multiplier stack: the Alternate Drag Speed setting still multiplies on top.
fn shift_drag_base(visible_seconds: f64, full_view_seconds: f64, frame_seconds: f64) -> f64 {
    /// multiplier of 1:1 at each fixed point (see the function docs)
    const AT_FRAME: f64 = 0.00025;
    const AT_ONE_SECOND: f64 = 0.00575;
    const AT_TEN_SECONDS: f64 = 0.02;
    const AT_MINUTE: f64 = 0.115;
    const AT_FULL_VIEW: f64 = 0.75;
    let log_lerp = |y0: f64, y1: f64, x: f64, x0: f64, x1: f64| {
        let f = if x.is_finite() && x.is_sign_positive() && x0.is_finite() && x1.is_finite() && x0 > 0.0 && x1 > x0 {
            ((x / x0).ln() / (x1 / x0).ln()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (y0.ln() + (y1.ln() - y0.ln()) * f).exp()
    };
    let frame = if frame_seconds.is_finite() { frame_seconds.clamp(0.001, 9.999) } else { 1.0 / 24.0 };
    if !(visible_seconds.is_finite() && visible_seconds > 0.0 && full_view_seconds.is_finite() && full_view_seconds > 0.0) {
        return AT_MINUTE;
    }
    if visible_seconds >= full_view_seconds {
        AT_FULL_VIEW
    } else if visible_seconds >= 60.0 {
        log_lerp(AT_MINUTE, AT_FULL_VIEW, visible_seconds, 60.0, full_view_seconds)
    } else if visible_seconds >= 10.0 {
        log_lerp(AT_TEN_SECONDS, AT_MINUTE, visible_seconds, 10.0, 60.0)
    } else if visible_seconds >= 1.0 {
        log_lerp(AT_ONE_SECOND, AT_TEN_SECONDS, visible_seconds, 1.0, 10.0)
    } else if visible_seconds >= frame {
        log_lerp(AT_FRAME, AT_ONE_SECOND, visible_seconds, frame, 1.0)
    } else {
        AT_FRAME
    }
}

/// How much faster a Shift drag on the zoom bar's EDGES runs than the tuned [`shift_drag_base`]
/// curve at that zoom level: 30x when only 0.1 s is in view, 5x at 1 s, 2x at 10 s, easing back to
/// the plain curve by a minute (so the full-view sweep stays a sweep). Log-interpolated between the
/// anchors, 30x below 0.1 s, 1.0 for hostile input. The slider's middle never uses this.
fn shift_edge_speed_multiplier(visible_seconds: f64) -> f64 {
    /// multiplier of the tuned curve at each fixed point (see the function docs)
    const AT_TENTH_SECOND: f64 = 30.0;
    const AT_ONE_SECOND: f64 = 5.0;
    const AT_TEN_SECONDS: f64 = 2.0;
    const AT_MINUTE: f64 = 1.0;
    let log_lerp = |y0: f64, y1: f64, x: f64, x0: f64, x1: f64| {
        let f = if x.is_finite() && x.is_sign_positive() && x0.is_finite() && x1.is_finite() && x0 > 0.0 && x1 > x0 {
            ((x / x0).ln() / (x1 / x0).ln()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (y0.ln() + (y1.ln() - y0.ln()) * f).exp()
    };
    if !(visible_seconds.is_finite() && visible_seconds > 0.0) {
        return 1.0;
    }
    if visible_seconds <= 0.1 {
        AT_TENTH_SECOND
    } else if visible_seconds < 1.0 {
        log_lerp(AT_TENTH_SECOND, AT_ONE_SECOND, visible_seconds, 0.1, 1.0)
    } else if visible_seconds < 10.0 {
        log_lerp(AT_ONE_SECOND, AT_TEN_SECONDS, visible_seconds, 1.0, 10.0)
    } else if visible_seconds < 60.0 {
        log_lerp(AT_TEN_SECONDS, AT_MINUTE, visible_seconds, 10.0, 60.0)
    } else {
        AT_MINUTE
    }
}

/// Extend a short edge run to the semicircle tangent instead of flattening its cap.
fn minimap_indicator_bounds(mut xa: f32, mut xb: f32, left_tangent: f32, right_tangent: f32, hug_left: bool, hug_right: bool) -> (f32, f32) {
    if hug_left {
        xb = xb.max(left_tangent);
    }
    if hug_right {
        xa = xa.min(right_tangent);
    }
    (xa, xb)
}

/// Give an end cap only to the outermost run near that edge, so adjacent runs keep their cut.
fn minimap_indicator_hugs_edges(index: usize, run_count: usize, left_gap: f32, right_gap: f32, cap_zone: f32) -> (bool, bool) {
    (index == 0 && left_gap < cap_zone, index + 1 == run_count && right_gap < cap_zone)
}

/// Fill narrow interior indicators, but keep edge indicators outlined unless Fill Clips is enabled.
fn minimap_indicator_filled(fill_clips: bool, width: f32, min_outline_width: f32, hug_left: bool, hug_right: bool) -> bool {
    fill_clips || (width < min_outline_width && !hug_left && !hug_right)
}

#[cfg(test)]
mod minimap_indicator_tests {
    use super::{MinimapCue, minimap_cue, minimap_indicator_bounds, minimap_indicator_filled, minimap_indicator_hugs_edges};

    #[test]
    fn audio_and_offline_minimap_runs_have_non_colour_cues() {
        assert_eq!(minimap_cue(2, false), MinimapCue::None);
        assert_eq!(minimap_cue(1, false), MinimapCue::Audio);
        assert_eq!(minimap_cue(1, true), MinimapCue::None);
        assert_eq!(minimap_cue(2, true), MinimapCue::Audio);
        assert_eq!(minimap_cue(3, false), MinimapCue::Offline);
        assert_eq!(minimap_cue(3, true), MinimapCue::Offline);
    }

    #[test]
    fn edge_caps_keep_a_full_semicircle_for_short_runs() {
        assert_eq!(minimap_indicator_bounds(20.0, 20.2, 24.0, 76.0, true, false), (20.0, 24.0));
        assert_eq!(minimap_indicator_bounds(79.8, 80.0, 24.0, 76.0, false, true), (76.0, 80.0));
        assert_eq!(minimap_indicator_bounds(40.0, 45.0, 24.0, 76.0, false, false), (40.0, 45.0));
    }

    #[test]
    fn only_outermost_short_runs_claim_edge_caps() {
        assert_eq!(minimap_indicator_hugs_edges(0, 2, 1.0, 50.0, 10.0), (true, false));
        assert_eq!(minimap_indicator_hugs_edges(1, 2, 5.0, 1.0, 10.0), (false, true));
        assert_eq!(minimap_indicator_hugs_edges(1, 3, 5.0, 50.0, 10.0), (false, false));
        assert_eq!(minimap_indicator_hugs_edges(0, 1, 1.0, 1.0, 10.0), (true, true));
    }

    #[test]
    fn narrow_semicircles_stay_outlined_unless_fill_clips_is_enabled() {
        assert!(!minimap_indicator_filled(false, 0.5, 3.0, true, false));
        assert!(!minimap_indicator_filled(false, 0.5, 3.0, false, true));
        assert!(minimap_indicator_filled(true, 0.5, 3.0, true, false));
        assert!(minimap_indicator_filled(false, 0.5, 3.0, false, false));
    }
}

#[cfg(test)]
mod zoom_drag_scale_tests {
    use super::{shift_drag_base, shift_edge_speed_multiplier};

    #[test]
    fn edge_speed_multiplier_hits_the_zoomed_in_anchors_and_survives_hostile_input() {
        // the tuned zoomed-in anchors: 30x the curve at 0.1 s of view, 5x at 1 s, 2x at 10 s
        assert!((shift_edge_speed_multiplier(0.1) - 30.0).abs() < 1e-9);
        assert!((shift_edge_speed_multiplier(1.0) - 5.0).abs() < 1e-9);
        assert!((shift_edge_speed_multiplier(10.0) - 2.0).abs() < 1e-9);
        // past a minute the plain curve is untouched: the full-view sweep stays a sweep
        assert!((shift_edge_speed_multiplier(60.0) - 1.0).abs() < 1e-9);
        assert!((shift_edge_speed_multiplier(1e6) - 1.0).abs() < 1e-9);
        // clamped below the smallest anchor
        assert!((shift_edge_speed_multiplier(0.01) - 30.0).abs() < 1e-9);
        // eased without jumps between the anchors
        let mid = shift_edge_speed_multiplier(0.4);
        assert!(mid > 5.0 && mid < 30.0, "{mid}");
        // hostile input falls back to the plain curve, nothing extreme
        assert!((shift_edge_speed_multiplier(f64::NAN) - 1.0).abs() < 1e-9);
        assert!((shift_edge_speed_multiplier(0.0) - 1.0).abs() < 1e-9);
        assert!((shift_edge_speed_multiplier(-3.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn shift_speed_hits_the_tuned_anchors_and_survives_hostile_input() {
        let frame = 1.0 / 24.0;
        // tuned fixed points: one frame in view is 1/4000 of 1:1 speed, a second 0.00575
        assert!((shift_drag_base(frame, 600.0, frame) - 0.00025).abs() < 1e-9);
        assert!((shift_drag_base(1.0, 600.0, frame) - 0.00575).abs() < 1e-9);
        assert!((shift_drag_base(10.0, 600.0, frame) - 0.02).abs() < 1e-9);
        assert!((shift_drag_base(60.0, 600.0, frame) - 0.115).abs() < 1e-9);
        // the whole Timeline in view sweeps, but still slower than 1:1
        assert!((shift_drag_base(600.0, 600.0, frame) - 0.75).abs() < 1e-9);
        assert!(shift_drag_base(30.0, 600.0, frame) > 0.02 && shift_drag_base(30.0, 600.0, frame) < 0.115);
        // one frame of a 15 second project: still the frame anchor, never below it
        assert!((shift_drag_base(1.0 / 24.0, 15.0, frame) - 0.00025).abs() < 1e-9);
        // a slower Alternate Drag Speed setting behaves proportionally at the site calling this
        assert!(shift_drag_base(frame, 600.0, frame) < shift_drag_base(10.0, 600.0, frame) * 3.0);
        // hostile input falls back to a middle value, nothing panics or goes extreme
        let fallback = shift_drag_base(f64::NAN, 600.0, frame);
        assert!(fallback.is_finite());
        assert_eq!(shift_drag_base(10.0, f64::NAN, frame), shift_drag_base(f64::NAN, 600.0, frame));
        assert!(shift_drag_base(2400.0, 600.0, frame).is_finite());
        assert_eq!(shift_drag_base(5.0, 600.0, f64::NAN), shift_drag_base(5.0, 600.0, frame));
    }

    #[test]
    fn the_thumb_is_its_smallest_exactly_at_the_maximum_zoom_level() {
        use super::{MIN_THUMB_W, timeline_extent, zoom_thumb};
        let bar = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 13.0));
        let view_w = 800.0f32;
        // the slider's smallest state is reached at the Maximum Zoom Level (the setting) and only
        // there: one step wider and it is already bigger, whatever the Timeline's length
        for seq in [600.0, 3600.0, 36000.0, 86400.0] {
            for min_view in [0.05, 0.1, 0.4, 0.8, 3.0] {
                let at = |secs: f64| zoom_thumb(bar, view_w, seq, 0.0, f64::from(view_w) / secs, min_view).1;
                let w = at(min_view);
                assert!((w - MIN_THUMB_W).abs() < 0.5, "seq {seq} min {min_view}: {w}");
                let just_out = at(min_view * 1.2);
                assert!(just_out > MIN_THUMB_W + 0.5, "seq {seq} min {min_view}: {just_out}");
                assert!(at(min_view * 4.0) > just_out, "seq {seq} min {min_view}");
                // the whole Timeline in view fills the bar
                assert!((at(timeline_extent(seq)) - bar.width()).abs() < 0.5, "seq {seq}");
            }
        }
        // hostile zooms stay inside the bar, nothing panics
        for pps in [0.0, -3.0, f64::NAN, 1e-30, 1e30] {
            let (_, w, _) = zoom_thumb(bar, view_w, 3600.0, 0.0, pps, 0.4);
            assert!(w.is_finite() && w >= MIN_THUMB_W.min(bar.width()) && w <= bar.width(), "{pps}: {w}");
        }
    }

    #[test]
    fn center_zoom_replaces_a_stale_scroll_target_with_the_computed_view() {
        let mut view = crate::state::TimelineView { pps: 10.0, target_pps: 30.0, scroll: 5.0, target_scroll: 40.0, ..Default::default() };
        super::zoom_about_center(&mut view, 100.0, 100.0, 110.0, 20.0, 0.1);
        assert_eq!(view.target_scroll, view.scroll, "the next easing step cannot pan back toward the stale target center");
    }
}

#[cfg(test)]
mod controls_tests {
    use super::{
        MINIMAP_MAX_COLUMNS, SHIFT_CONTENT_RATE, SHIFT_THUMB_RATE, binding_matches_held_key, cancel_scroll_animation_for_manual_drag, controls_listen_key,
        controls_listening, input_text, minimap_clips_fit, minimap_column_count, minimap_coverage, minimap_runs, shift_drag_rate, zoom_scrollbar,
        zoombar_popup_open_key,
    };
    use crate::FilmcraftApp;

    /// The minimap's clips vanish below a quarter of the screen's width -- exactly at the quarter
    /// they still show -- and nothing else (the width alone decides).
    #[test]
    fn minimap_clips_vanish_below_a_quarter_of_the_screen() {
        assert!(!minimap_clips_fit(249.0, 1000.0), "a hair under the quarter is hidden");
        assert!(minimap_clips_fit(250.0, 1000.0), "exactly the quarter shows");
        assert!(minimap_clips_fit(800.0, 1000.0));
        assert!(!minimap_clips_fit(0.0, 1000.0));
        // a hostile screen width can never divide by zero or flip the answer
        assert!(minimap_clips_fit(10.0, 0.0) == minimap_clips_fit(10.0, 1.0));
    }

    #[test]
    fn minimap_column_allocation_is_bounded_for_hostile_geometry() {
        assert_eq!(minimap_column_count(0.0), 1);
        assert_eq!(minimap_column_count(640.0), 640);
        assert_eq!(minimap_column_count(f32::INFINITY), 1);
        assert_eq!(minimap_column_count(f32::MAX), MINIMAP_MAX_COLUMNS);
    }

    #[test]
    fn custom_zoom_popup_keeps_shortcut_capture_listening() {
        let app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        ctx.data_mut(|data| {
            data.insert_temp(zoombar_popup_open_key(), true);
            data.insert_temp(controls_listen_key(), Some(0usize));
        });
        assert!(controls_listening(&app, &ctx), "the custom Area popup is open even with no Popup positions set");
    }

    #[test]
    fn switching_to_track_settings_resets_zoom_capture_and_flyouts() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        app.tl.zoombar_settings_pos = Some(egui::pos2(10.0, 10.0));
        ctx.data_mut(|data| data.insert_temp(controls_listen_key(), Some(0usize)));
        assert!(controls_listening(&app, &ctx));

        super::zoombar_state_reset(&ctx);

        assert!(!controls_listening(&app, &ctx));
    }

    #[test]
    fn settings_reset_uses_the_registered_scoped_flyout_id() {
        let ctx = egui::Context::default();
        let actual_id = egui::Id::unique("popup-scope").with("timeline-zoombar-advanced-flyout");
        super::remember_flyout_id(&ctx, actual_id);
        ctx.data_mut(|data| data.insert_temp(actual_id.with("open"), true));

        super::zoombar_state_reset(&ctx);

        assert_eq!(ctx.data(|data| data.get_temp::<bool>(actual_id.with("open"))), Some(false));
    }

    #[test]
    fn leaving_edit_mode_clears_a_controls_capture() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        app.ui.mode = crate::state::Mode::Export;
        app.tl.zoombar_settings_pos = Some(egui::pos2(10.0, 10.0));
        ctx.data_mut(|data| data.insert_temp(controls_listen_key(), Some(0usize)));

        assert!(!controls_listening(&app, &ctx));
        assert!(!ctx.data(|data| data.get_temp::<Option<usize>>(controls_listen_key()).flatten().is_some()));
    }

    #[test]
    fn zoom_handles_have_stable_automation_ids() {
        use egui::{Context, RawInput, Rect, pos2, vec2};
        use serde_json::json;

        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let id = session.state.active_sequence.unwrap();
        let project = session.project.clone();
        let seq = project.sequence(id).unwrap();
        let mut app = FilmcraftApp::new(session);
        let tokens = app.tokens;
        let ctx = Context::default();
        let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..RawInput::default() };
        let snapshot_revision = app.session.revision;
        let mut output = ctx.run_ui(input, |ui| {
            zoom_scrollbar(&mut app, ui, Rect::from_min_size(pos2(50.0, 100.0), vec2(500.0, 16.0)), 500.0, seq, snapshot_revision, 1.0 / 24.0, &tokens);
        });
        output.textures_delta.clear();
        assert!(app.auto.find_current("timeline.zoomBar.handle.left").is_some());
        assert!(app.auto.find_current("timeline.zoomBar.handle.right").is_some());
    }

    #[test]
    fn paste_events_are_normalized_for_slider_shortcut_dispatch() {
        use egui::{Context, Event, Modifiers, RawInput};
        let ctx = Context::default();
        let input = RawInput { events: vec![Event::ModifiersChanged(Modifiers::CTRL), Event::Paste("x".into())], ..RawInput::default() };
        let mut output = ctx.run_ui(input, |ui| assert_eq!(input_text(ui, true).as_deref(), Some("Ctrl + V")));
        output.textures_delta.clear();
    }

    #[test]
    fn clipboard_only_paste_dispatches_zoom_binding() {
        use egui::{Context, Event, Modifiers, RawInput, Rect, pos2, vec2};
        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", serde_json::json!({})).unwrap();
        let id = session.state.active_sequence.unwrap();
        let project = session.project.clone();
        let seq = project.sequence(id).unwrap();
        let mut app = FilmcraftApp::new(session);
        app.session.state.timeline_ui.control_center_on_playhead = "Ctrl + V".into();
        app.ui.timeline.pps = 1.0;
        app.ui.timeline.target_pps = 1.0;
        let tokens = app.tokens;
        let ctx = Context::default();
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                events: vec![Event::ModifiersChanged(Modifiers::CTRL), Event::Paste("x".into())],
                ..RawInput::default()
            },
            |ui| zoom_scrollbar(&mut app, ui, Rect::from_min_size(pos2(50.0, 100.0), vec2(500.0, 16.0)), 500.0, seq, 0, 1.0 / 24.0, &tokens),
        );
        output.textures_delta.clear();
        assert_ne!(app.ui.timeline.target_pps, 1.0, "clipboard-only Ctrl+V dispatches the configured center action");
    }

    #[test]
    fn captured_copy_and_cut_clipboard_events_are_suppressed() {
        use egui::{Context, Event, Modifiers, RawInput};
        let ctx = Context::default();
        for (event, modifiers, chord) in [
            (Event::Copy, Modifiers::CTRL, "Ctrl + C"),
            (Event::Cut, Modifiers::CTRL, "Ctrl + X"),
            (Event::Copy, Modifiers::CTRL | Modifiers::SHIFT, "Ctrl + Shift + C"),
        ] {
            let mut output = ctx.run_ui(RawInput { events: vec![Event::ModifiersChanged(modifiers), event], ..RawInput::default() }, |ui| {
                ui.input_mut(|input| input.modifiers = modifiers);
                super::suppress_captured_chord(ui, chord);
                assert!(!ui.input(|input| input.events.iter().any(|event| matches!(event, Event::Copy | Event::Cut))));
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn zoom_popup_preset_action_updates_the_configured_ceiling() {
        let mut settings = filmcraft_engine::project::TimelineUiSettings::default();
        super::apply_max_zoom_choice(&mut settings, 3, &[0.1, 0.2, 0.4, 0.8]);
        assert_eq!(settings.max_zoom_seconds, 0.8);
        assert!(!settings.max_zoom_custom);
    }

    #[test]
    fn zoom_popup_preset_widget_updates_the_configured_ceiling() {
        use egui::{Context, Event, PointerButton, RawInput, Rect, pos2, vec2};
        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", serde_json::json!({})).unwrap();
        let mut app = FilmcraftApp::new(session);
        app.session.state.timeline_ui.max_zoom_seconds = 0.4;
        app.last_timeline_width = 500.0;
        app.ui.timeline.pps = 2000.0;
        app.ui.timeline.target_pps = 3000.0;
        app.ui.timeline.scroll = 100.0;
        app.ui.timeline.target_scroll = 200.0;
        let ctx = Context::default();
        app.auto.begin_frame();
        let screen = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0)));
        let mut output = ctx.run_ui(RawInput { screen_rect: screen, ..RawInput::default() }, |ui| super::zoombar_settings(&mut app, ui));
        output.textures_delta.clear();
        let current_center = app.ui.timeline.scroll + 250.0 / app.ui.timeline.pps;
        let target_center = app.ui.timeline.target_scroll + 250.0 / app.ui.timeline.target_pps;
        let [x, y, width, height] = app.auto.find_current("timeline.zoomBar.settings.maxZoomLevel.slider").unwrap().rect;
        let point = pos2(x + width * 0.75, y + height * 0.5);
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: screen,
                events: vec![
                    Event::PointerMoved(point),
                    Event::PointerButton { pos: point, button: PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
                ],
                ..RawInput::default()
            },
            |ui| super::zoombar_settings(&mut app, ui),
        );
        output.textures_delta.clear();
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: screen,
                events: vec![
                    Event::PointerMoved(point),
                    Event::PointerButton { pos: point, button: PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
                ],
                ..RawInput::default()
            },
            |ui| super::zoombar_settings(&mut app, ui),
        );
        output.textures_delta.clear();
        assert_eq!(app.session.state.timeline_ui.max_zoom_seconds, 0.8);
        assert!(!app.session.state.timeline_ui.max_zoom_custom);
        assert!((app.ui.timeline.scroll + 250.0 / app.ui.timeline.pps - current_center).abs() < 1e-6, "the popup change preserves the current viewport center");
        assert!(
            (app.ui.timeline.target_scroll + 250.0 / app.ui.timeline.target_pps - target_center).abs() < 1e-6,
            "the popup change preserves the target viewport center"
        );
    }

    #[test]
    fn collapsed_timeline_width_does_not_reduce_saved_zoom() {
        use egui::{Context, RawInput, Rect, pos2, vec2};
        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", serde_json::json!({})).unwrap();
        let mut app = FilmcraftApp::new(session);
        app.last_timeline_width = 1.0;
        app.ui.timeline.pps = 1000.0;
        app.ui.timeline.target_pps = 1200.0;
        let ctx = Context::default();
        let mut output = ctx.run_ui(RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..RawInput::default() }, |ui| {
            super::zoombar_settings(&mut app, ui)
        });
        output.textures_delta.clear();
        assert_eq!(app.ui.timeline.pps, 1000.0);
        assert_eq!(app.ui.timeline.target_pps, 1200.0);
    }

    #[test]
    fn escape_closes_zoom_popup_but_leaves_capture_escape_to_cancel_capture() {
        assert!(super::should_close_zoombar_popup_on_escape(true, true, false));
        assert!(!super::should_close_zoombar_popup_on_escape(true, true, true));
        assert!(!super::should_close_zoombar_popup_on_escape(true, false, false));
    }

    #[test]
    fn pending_zoom_setting_flushes_after_timeline_stops_drawing() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let id = egui::Id::unique("tl-zoombar-view-pending");
        ctx.data_mut(|data| data.insert_temp(id, app.session.document_epoch));
        let old_epoch = app.session.view_epoch;
        super::flush_pending_zoombar_view_change(&mut app, &ctx);
        assert!(app.session.view_epoch > old_epoch, "the setting change reaches the persisted view");
        assert!(ctx.data(|data| data.get_temp::<u64>(id)).is_none(), "the pending flag is consumed once");
    }

    #[test]
    fn pending_track_slider_setting_flushes_from_app_context() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let id = egui::Id::unique("timeline-track-slider-view-pending");
        ctx.data_mut(|data| data.insert_temp(id, app.session.document_epoch));
        let old_epoch = app.session.view_epoch;
        super::flush_track_slider_view_change(&mut app, &ctx);
        assert!(app.session.view_epoch > old_epoch, "the app-level context flush persists the hidden popup's pending setting");
        assert!(ctx.data(|data| data.get_temp::<u64>(id)).is_none());
    }

    #[test]
    fn close_flushes_pending_view_changes_even_while_a_pointer_is_held() {
        use egui::{Event, PointerButton, RawInput, pos2};
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let zoom_id = egui::Id::unique("tl-zoombar-view-pending");
        let track_id = egui::Id::unique("timeline-track-slider-view-pending");
        ctx.data_mut(|data| {
            data.insert_temp(zoom_id, app.session.document_epoch);
            data.insert_temp(track_id, app.session.document_epoch);
        });
        let mut output = ctx.run_ui(
            RawInput {
                events: vec![Event::PointerButton { pos: pos2(5.0, 5.0), button: PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }],
                ..RawInput::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        let old_epoch = app.session.view_epoch;
        super::flush_pending_view_changes_for_close(&mut app, &ctx);
        assert!(app.session.view_epoch > old_epoch);
        assert!(ctx.data(|data| data.get_temp::<u64>(zoom_id)).is_none());
        assert!(ctx.data(|data| data.get_temp::<u64>(track_id)).is_none());
    }

    #[test]
    fn track_shortcuts_are_reserved_only_over_a_visible_slider() {
        use egui::{Event, Key, Modifiers, RawInput, Rect, pos2, vec2};

        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", serde_json::json!({})).unwrap();
        let mut app = FilmcraftApp::new(session);
        app.session.state.timeline_ui.track_control_copy_slider = "K".into();
        app.session.state.timeline_ui.control_settings = "S".into();
        app.auto.add("timeline.trackSlider.video.bar", Rect::from_min_size(pos2(10.0, 20.0), vec2(40.0, 80.0)), "video slider");
        app.auto.add("timeline.zoomBar.track", Rect::from_min_size(pos2(100.0, 20.0), vec2(40.0, 16.0)), "zoom bar");
        app.auto.begin_frame();
        let ctx = egui::Context::default();
        let frame = |pos, key| RawInput {
            events: vec![Event::PointerMoved(pos), Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: Modifiers::NONE }],
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(frame(pos2(20.0, 30.0), Key::K), |_| assert!(super::custom_control_shortcut_pending(&app, &ctx)));
        output.textures_delta.clear();
        let mut output = ctx.run_ui(frame(pos2(200.0, 30.0), Key::K), |_| assert!(!super::custom_control_shortcut_pending(&app, &ctx)));
        output.textures_delta.clear();
        app.session.state.timeline_ui.track_slider_hidden = true;
        let mut output = ctx.run_ui(frame(pos2(20.0, 30.0), Key::K), |_| assert!(!super::custom_control_shortcut_pending(&app, &ctx)));
        output.textures_delta.clear();
        let mut output = ctx.run_ui(frame(pos2(200.0, 30.0), Key::S), |_| assert!(!super::custom_control_shortcut_pending(&app, &ctx)));
        output.textures_delta.clear();
        let mut output = ctx.run_ui(frame(pos2(110.0, 25.0), Key::S), |_| assert!(super::custom_control_shortcut_pending(&app, &ctx)));
        output.textures_delta.clear();

        app.session.state.timeline_ui.control_alternate_drag_speed = "Shift + K".into();
        let alternate = vec![
            Event::PointerMoved(pos2(200.0, 30.0)),
            Event::Key { key: Key::K, physical_key: Some(Key::K), pressed: true, repeat: false, modifiers: Modifiers::SHIFT },
        ];
        let mut output = ctx.run_ui(RawInput { events: alternate.clone(), ..RawInput::default() }, |_| {
            assert!(!super::custom_control_shortcut_pending_for_events(&app, &ctx, &alternate, Modifiers::SHIFT));
        });
        output.textures_delta.clear();
        app.tl.drag = Some(super::Drag::ZoomBar { grab: 0.0, start: 0.0, start_pps: 1.0, mode: 0 });
        let mut output = ctx.run_ui(RawInput { events: alternate.clone(), ..RawInput::default() }, |_| {
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &alternate, Modifiers::SHIFT));
        });
        output.textures_delta.clear();
        app.tl.drag = None;

        app.session.state.timeline_ui.control_center_on_playhead = "Ctrl + V".into();
        app.session.state.timeline_ui.zoom_bar_hidden = false;
        let paste = vec![Event::PointerMoved(pos2(110.0, 25.0)), Event::Paste("clipboard text".into())];
        let paste_image =
            vec![Event::PointerMoved(pos2(110.0, 25.0)), Event::PasteImage(std::sync::Arc::new(egui::ColorImage::filled([1, 1], egui::Color32::RED)))];
        let input = RawInput { events: paste.clone(), ..RawInput::default() };
        let mut output = ctx.run_ui(input, |_| {
            let key = vec![Event::Key { key: Key::V, physical_key: Some(Key::V), pressed: true, repeat: false, modifiers: Modifiers::CTRL }];
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &key, Modifiers::CTRL));
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &paste, Modifiers::CTRL));
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &paste, Modifiers::CTRL | Modifiers::COMMAND));
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &paste_image, Modifiers::CTRL));
        });
        output.textures_delta.clear();
        let multiple = vec![
            Event::PointerMoved(pos2(110.0, 25.0)),
            Event::Key { key: Key::K, physical_key: Some(Key::K), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
            Event::Key { key: Key::J, physical_key: Some(Key::J), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
        ];
        app.session.state.timeline_ui.control_center_on_playhead = "Ctrl + K".into();
        let mut output = ctx.run_ui(RawInput { events: multiple.clone(), ..RawInput::default() }, |ui| {
            assert_eq!(input_text(ui, true).as_deref(), Some("Ctrl + J"));
            assert!(!super::custom_control_shortcut_pending_for_events(&app, &ctx, &multiple, Modifiers::CTRL));
        });
        output.textures_delta.clear();
        app.session.state.timeline_ui.control_center_on_playhead = "Ctrl + J".into();
        let mut output = ctx.run_ui(RawInput { events: multiple.clone(), ..RawInput::default() }, |_| {
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &multiple, Modifiers::CTRL));
        });
        output.textures_delta.clear();
    }

    #[test]
    fn alternate_speed_binding_is_reserved_only_during_the_track_slider_drag() {
        use egui::{Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", serde_json::json!({})).unwrap();
        let mut app = FilmcraftApp::new(session);
        app.session.state.timeline_ui.track_control_alternate_drag_speed = "Ctrl + K".into();
        app.auto.add("timeline.trackSlider.video.bar", Rect::from_min_size(pos2(10.0, 20.0), vec2(40.0, 80.0)), "video slider");
        let ctx = egui::Context::default();
        let events = vec![
            Event::PointerMoved(pos2(20.0, 30.0)),
            Event::Key { key: Key::K, physical_key: Some(Key::K), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
        ];
        let mut output = ctx.run_ui(RawInput { events: events.clone(), ..RawInput::default() }, |_| {
            assert!(!super::custom_control_shortcut_pending_for_events(&app, &ctx, &events, Modifiers::CTRL));
        });
        output.textures_delta.clear();
        app.tl.track_slider_dragging = true;
        let outside = vec![
            Event::PointerMoved(pos2(200.0, 200.0)),
            Event::Key { key: Key::K, physical_key: Some(Key::K), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
        ];
        let mut output = ctx.run_ui(RawInput { events: outside.clone(), ..RawInput::default() }, |_| {
            assert!(super::custom_control_shortcut_pending_for_events(&app, &ctx, &outside, Modifiers::CTRL));
        });
        output.textures_delta.clear();
    }

    #[test]
    fn custom_zoom_binding_does_not_reserve_save_without_an_active_sequence() {
        use egui::{Context, Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        app.session.state.timeline_ui.control_center_on_playhead = "Ctrl + S".into();
        app.auto.add("timeline.zoomBar.track", Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 20.0)), "zoom bar");
        let ctx = Context::default();
        let input = RawInput {
            events: vec![
                Event::PointerMoved(pos2(20.0, 15.0)),
                Event::Key { key: Key::S, physical_key: Some(Key::S), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
            ],
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(input.clone(), |_| {
            assert!(!super::custom_control_shortcut_pending_for_events(&app, &ctx, &input.events, Modifiers::CTRL));
        });
        output.textures_delta.clear();
    }

    #[test]
    fn custom_timeline_binding_is_not_reserved_outside_edit_mode() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        assert!(super::timeline_consumer_available(&app));
        app.ui.mode = crate::state::Mode::Import;
        assert!(!super::timeline_consumer_available(&app));
        app.ui.mode = crate::state::Mode::Export;
        assert!(!super::timeline_consumer_available(&app));
        app.ui.mode = crate::state::Mode::Edit;
        app.ui.fullscreen_program = true;
        assert!(!super::timeline_consumer_available(&app), "fullscreen Program mode does not draw the Timeline control");
    }

    #[test]
    fn captured_track_shortcut_is_removed_before_later_slider_dispatch() {
        use egui::{Context, Event, Key, Modifiers, RawInput};
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = Context::default();
        ctx.data_mut(|data| data.insert_temp(controls_listen_key(), Some(4usize)));
        let input = RawInput {
            events: vec![
                Event::Key { key: Key::P, physical_key: Some(Key::P), pressed: true, repeat: false, modifiers: Modifiers::NONE },
                Event::Key { key: Key::K, physical_key: Some(Key::K), pressed: true, repeat: false, modifiers: Modifiers::CTRL },
            ],
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            super::capture_controls(&mut app, ui);
            assert!(ui.input(|i| i.events.iter().any(|event| matches!(event, Event::Key { key: Key::P, .. }))));
            assert!(!ui.input(|i| i.events.iter().any(|event| matches!(event, Event::Key { key: Key::K, .. }))));
        });
        output.textures_delta.clear();
        assert_eq!(app.session.state.timeline_ui.track_control_copy_slider, "Ctrl + K");
    }

    #[test]
    fn stale_pending_settings_do_not_change_a_replacement_document() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let zoom_id = egui::Id::unique("tl-zoombar-view-pending");
        let track_id = egui::Id::unique("timeline-track-slider-view-pending");
        let previous_document = app.session.document_epoch;
        ctx.data_mut(|data| {
            data.insert_temp(zoom_id, previous_document);
            data.insert_temp(track_id, previous_document);
        });
        app.session.document_epoch = previous_document.wrapping_add(1);
        let old_epoch = app.session.view_epoch;
        super::flush_pending_view_changes_for_close(&mut app, &ctx);
        assert_eq!(app.session.view_epoch, old_epoch);
        assert!(ctx.data(|data| data.get_temp::<u64>(zoom_id)).is_none());
        assert!(ctx.data(|data| data.get_temp::<u64>(track_id)).is_none());
    }

    #[test]
    fn closed_zoom_popup_flushes_a_pending_view_change() {
        use egui::{Context, RawInput, Rect, pos2, vec2};
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = Context::default();
        let id = egui::Id::unique("tl-zoombar-view-pending");
        ctx.data_mut(|data| data.insert_temp(id, app.session.document_epoch));
        let before = app.session.view_epoch;
        let mut output = ctx.run_ui(RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))), ..RawInput::default() }, |_| {});
        output.textures_delta.clear();
        super::flush_pending_zoombar_view_change(&mut app, &ctx);
        assert!(app.session.view_epoch > before);
        assert!(ctx.data(|data| data.get_temp::<u64>(id)).is_none());
    }

    #[test]
    fn minimap_keeps_adjacent_same_class_clip_cuts() {
        assert_eq!(minimap_runs(&[2, 2, 2, 2], &[2]), vec![(0, 2, 2), (2, 4, 2)]);
        assert_eq!(minimap_runs(&[2, 2, 0, 2, 2], &[]), vec![(0, 2, 2), (3, 5, 2)]);
    }

    #[test]
    fn minimap_suppression_requires_a_clip_to_cross_the_cut() {
        let cut = filmcraft_time::Tick::from_seconds_f64(5.0);
        assert!(!super::clip_crosses_cut(cut, filmcraft_time::Tick::from_seconds_f64(10.0), cut));
        assert!(!super::clip_crosses_cut(filmcraft_time::Tick::ZERO, cut, cut));
        assert!(super::clip_crosses_cut(filmcraft_time::Tick::from_seconds_f64(4.0), filmcraft_time::Tick::from_seconds_f64(6.0), cut));
    }

    #[test]
    fn production_minimap_suppresses_only_cuts_crossed_by_another_track() {
        use filmcraft_project::{Project, Sequence, SequenceSettings, Track, TrackId, TrackItem, TrackKind};
        let item = |id, start, duration| {
            let start = filmcraft_time::Tick::from_seconds_f64(start);
            let duration = filmcraft_time::Tick::from_seconds_f64(duration);
            serde_json::from_value::<TrackItem>(serde_json::json!({
                "id": id, "item": id, "name": "fixture", "label": serde_json::to_value(filmcraft_project::Label::Violet).unwrap(),
                "start": start, "duration": duration, "source_in": 0, "speed": 1.0, "enabled": true, "effects": []
            }))
            .unwrap()
        };
        let mut sequence = Sequence {
            settings: SequenceSettings::default(),
            video_tracks: vec![Track::new(TrackId(1), TrackKind::Video, "V1".into()), Track::new(TrackId(2), TrackKind::Video, "V2".into())],
            audio_tracks: vec![],
            markers: vec![],
            mark_in: None,
            mark_out: None,
            work_area: None,
            start_timecode: 0,
            master_volume_db: 0.0,
            master_effects: vec![],
            master_mixer: Default::default(),
            submix_tracks: vec![],
            caption_tracks: vec![],
            multicam: None,
            merged: None,
            split: Default::default(),
        };
        sequence.video_tracks[0].items = vec![item(1, 0.0, 5.0), item(2, 5.0, 5.0)];
        sequence.video_tracks[1].items = vec![item(3, 4.0, 2.0)];
        let (class, cuts) = minimap_coverage(&sequence, &Project::new("fixture"), 10.0, 10.0, 10, false);
        assert_eq!(class.len(), 10);
        assert!(cuts.is_empty(), "a later-track clip crossing the cut hides it");

        sequence.video_tracks[1].items = vec![item(3, 5.0, 2.0)];
        let (_, cuts) = minimap_coverage(&sequence, &Project::new("fixture"), 10.0, 10.0, 10, false);
        assert_eq!(cuts, vec![5], "a clip that only touches the cut does not hide it");
    }

    #[test]
    fn manual_scroll_drag_starts_from_the_displayed_scroll_and_zoom() {
        let mut view = crate::state::TimelineView { scroll: 12.0, target_scroll: 20.0, pps: 4.0, target_pps: 8.0, ..Default::default() };
        cancel_scroll_animation_for_manual_drag(&mut view);
        assert_eq!(view.target_scroll, 12.0);
        assert_eq!(view.target_pps, 4.0);
    }

    #[test]
    fn custom_alternate_speed_binding_requires_the_saved_chord() {
        assert!(binding_matches_held_key("Ctrl + Shift + K", "K", true, true, false));
        assert!(!binding_matches_held_key("Ctrl + Shift + K", "K", false, true, false));
        assert!(!binding_matches_held_key("Ctrl + Shift + K", "J", true, true, false));
        assert!(binding_matches_held_key("K", "K", false, false, false));
        assert!(!binding_matches_held_key("Right Click", "K", false, false, false));
    }

    /// The Alternate Drag Speed setting squares: 1x keeps the slow rate, 0.5x a quarter of it and
    /// Custom 10x a hundred times it -- nothing caps at 1x.
    #[test]
    fn the_speed_setting_squares_and_never_caps_at_one() {
        let one = shift_drag_rate(1.0);
        assert!((shift_drag_rate(0.5) - one * 0.25).abs() < 1e-9);
        assert!((shift_drag_rate(10.0) - one * 100.0).abs() < 1e-9);
        assert!(shift_drag_rate(10.0) > shift_drag_rate(1.0));
        // hostile settings fall back to a slow rate, never zero or NaN
        assert!(shift_drag_rate(f32::NAN) > 0.0);
        assert!(shift_drag_rate(0.0) > 0.0);
    }

    /// Both Shift rates are fixed constants times the same setting term, so a 12 track video
    /// slider and a 3 track audio slider move 1:1 with each other -- there is no per-slider input
    /// left to skew them -- and the scroll rate is a little over three times the height rate.
    #[test]
    fn the_shift_rates_are_constants_so_the_sliders_feel_one_to_one() {
        let one = shift_drag_rate(1.0);
        let ten = shift_drag_rate(10.0);
        assert!(SHIFT_THUMB_RATE * one > 0.0 && SHIFT_CONTENT_RATE * one > 0.0);
        assert!((SHIFT_CONTENT_RATE * one / (SHIFT_THUMB_RATE * one) - 10.0 / 3.0).abs() < 0.01);
        // the setting scales both rates together, never one slider's alone
        assert!((SHIFT_CONTENT_RATE * ten / (SHIFT_CONTENT_RATE * one) - 100.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod track_thumb_tests {
    use super::{centered_track_scroll, track_height_of, track_slider_click_count, track_thumb};

    #[test]
    fn track_thumb_fits_short_bars_and_inverse_stays_finite() {
        for bar_height in [2.0, 8.0, 20.0, 33.0, 34.0, 80.0] {
            let thumb = track_thumb(80.0, bar_height);
            assert!(thumb.is_finite() && (0.0..=bar_height).contains(&thumb));
            let track_height = track_height_of(thumb, bar_height);
            assert!(track_height.is_finite() && (super::super::MIN_TRACK_HEIGHT..=super::super::MAX_TRACK_HEIGHT).contains(&track_height));
        }
        assert_eq!(track_thumb(80.0, f32::NAN), 0.0);
    }

    #[test]
    fn track_height_slider_round_trips_the_shared_keyboard_maximum() {
        let thumb = track_thumb(super::super::MAX_TRACK_HEIGHT, 80.0);
        assert!((track_height_of(thumb, 80.0) - super::super::MAX_TRACK_HEIGHT).abs() < 0.01);
    }

    #[test]
    fn one_point_edge_drag_preserves_a_track_height_above_the_old_maximum() {
        let max = super::super::MAX_TRACK_HEIGHT;
        let bar = 200.0;
        let thumb = track_thumb(max, bar);
        let adjusted = track_height_of((thumb + 1.0).clamp(super::MIN_THUMB_W, bar), bar);
        assert!(adjusted > 220.0, "a one-point edge movement from {max} should not collapse to the old ceiling: {adjusted}");
    }

    #[test]
    fn custom_copy_chord_wins_over_shift_minimum_actions() {
        assert_eq!(track_slider_click_count(false, true, false, true, true), 1);
        assert_eq!(track_slider_click_count(false, false, true, true, true), 2);
        assert_eq!(track_slider_click_count(true, false, false, true, true), 3);
    }

    #[test]
    fn height_zoom_keeps_the_content_under_the_viewport_center() {
        let scroll = centered_track_scroll(500.0, 300.0, 220.0, 69.57, 10.0 * 69.57 - 300.0);
        assert!((scroll - 55.55).abs() < 0.1, "center-preserving scroll was {scroll}");
        assert_eq!(centered_track_scroll(500.0, 300.0, 220.0, 69.57, 20.0), 20.0, "the adjusted scroll is clamped to its new range");
    }
}

#[cfg(test)]
mod color_picker_tests {
    use super::{closest_six, sync_external_color_buffer};

    #[test]
    fn hostile_hex_input_never_panics() {
        // clearing the hex field panicked here once: "index out of bounds: the len is 0 but the
        // index is 0" -- no digits at all must be a plain fallback, never a crash
        assert_eq!(closest_six(""), "#000000");
        assert_eq!(closest_six(&"abcdef".repeat(100_000)), "#abcdef");
        // three digits expand the CSS way, longer ones are cut to six, shorter ones filled out
        assert_eq!(closest_six("abc"), "#aabbcc");
        assert_eq!(closest_six("1122334455"), "#112233");
        assert_eq!(closest_six("74b0ff"), "#74b0ff");
        assert_eq!(closest_six("AB"), "#ababab");
    }

    #[test]
    fn external_color_changes_sync_after_the_hex_field_loses_focus() {
        let mut hex = "#111111".to_string();
        let mut observed = "#111111".to_string();
        sync_external_color_buffer(&mut hex, &mut observed, "#222222", true);
        assert_eq!(hex, "#111111", "do not interrupt active typing");
        sync_external_color_buffer(&mut hex, &mut observed, "#222222", false);
        assert_eq!(hex, "#222222", "show the current project setting when editing ends");
        assert_eq!(observed, "#222222");
    }
}

fn sync_external_color_buffer(hex: &mut String, observed: &mut String, color: &str, field_focused: bool) {
    if !field_focused && *observed != color {
        *hex = color.to_string();
    }
    if !field_focused {
        *observed = color.to_string();
    }
}

// the zoom bar's gestures are one state machine (modes, cursor pin, settings popups, minimap);
// splitting it would separate state that must stay in sync within a single frame
#[allow(clippy::cognitive_complexity, clippy::too_many_arguments)]
// These values are live inputs from the timeline renderer; wrapping them would obscure their distinct roles.
pub(super) fn zoom_scrollbar(
    app: &mut FilmcraftApp,
    ui: &mut egui::Ui,
    bar: Rect,
    view_w: f32,
    seq: &Sequence,
    snapshot_revision: u64,
    frame_seconds: f64,
    t: &Tokens,
) {
    let seq_seconds = seq.duration().seconds();
    let p = ui.painter();
    let resp = ui.interact(bar, egui::Id::unique("tl-zoombar"), Sense::click_and_drag());
    p.rect_filled(bar, bar.height() / 2.0, t.separator);
    let vis = view_w.max(1.0) as f64 / app.ui.timeline.pps.max(1e-9);
    let total = timeline_extent(seq_seconds).max(vis);
    let min_view = app.session.state.timeline_ui.max_zoom_secs();
    let (left, w, per_point) = zoom_thumb(bar, view_w, seq_seconds, app.ui.timeline.scroll, app.ui.timeline.pps, min_view);
    // Slider Minimap (draft): a NON-detailed map of where clips are on the whole Timeline. The
    // clips are laid out over the minimap's span and never move when the view zooms or pans -- the
    // thumb is the control, not the viewport indicator. Clips that overlap in time (a clip with
    // one on top of it, in any track) read as ONE rectangle; every rectangle has a 1 pt gap to the
    // next, so each cut shows as a vertical line across the whole minimap. Show Whole Timeline (on
    // by default) locks the span to the full Timeline extent (the sequence plus its ten minute
    // tail), so the "you are here" line can travel past the last clip; off, the map stops at the
    // end of the sequence.
    let minimap_span = if app.session.state.timeline_ui.show_whole_timeline {
        timeline_extent(seq_seconds).max(1e-9)
    } else if seq_seconds.is_finite() && seq_seconds > 0.0 {
        seq_seconds
    } else {
        1e-9
    };
    let minimap_x = |t: f64| -> f32 {
        let t = if t.is_finite() { t.clamp(0.0, minimap_span) } else { 0.0 };
        (bar.min.x as f64 + (t / minimap_span) * f64::from(bar.width())) as f32
    };
    let thumb = Rect::from_min_max(pos2(left, bar.min.y), pos2(left + w, bar.max.y));
    // lit while the pointer is over the thumb, and for as long as any zoom-bar drag is going on --
    // dragging by an edge handle moves the pointer off the thumb, and the thumb must not flash grey
    // while it is being worked (the highlight follows the drag, not just the pointer)
    let hover = ui.rect_contains_pointer(thumb) || matches!(app.tl.drag, Some(Drag::ZoomBar { .. })) && resp.dragged();
    p.rect_filled(thumb, thumb.height() / 2.0, if hover { t.scroll_thumb_hover } else { t.scroll_thumb });
    // Below 25% of the SCREEN's width the minimap's clip indicators disappear (they become
    // unreadable that narrow): a fresh decision every frame from the bar's width alone, so it can
    // never stick. The window's height never enters into it. The position indicator stays.
    let minimap_room = minimap_clips_fit(bar.width(), ui.ctx().content_rect().width());
    if app.session.state.timeline_ui.slider_minimap && minimap_room {
        // One class per x column: 0 empty, 1 the lower-priority kind, 2 the higher, 3 offline
        // media (always on top, whatever Prioritise Audio says). The covered kind's rectangle
        // simply ends where the winner begins. Drawn after the thumb (slightly transparent), so
        // clips never disappear behind the slider.
        let cols = minimap_column_count(bar.width());
        let pri_audio = app.session.state.timeline_ui.prioritise_audio;
        let sequence_id = app.session.state.active_sequence.unwrap_or_default();
        let cache_key = (sequence_id, snapshot_revision, cols, bar.width().to_bits(), pri_audio, app.session.state.timeline_ui.show_whole_timeline);
        if app.tl.minimap_cache.as_ref().is_none_or(|cache| cache.key != cache_key) {
            let (class, adjacent_cuts) = minimap_coverage(seq, &app.session.project, minimap_span, bar.width(), cols, pri_audio);
            app.tl.minimap_cache = Some(MinimapCache { key: cache_key, class, adjacent_cuts });
        }
        let (class, adjacent_cuts) = app.tl.minimap_cache.as_ref().map_or((&[][..], &[][..]), |cache| (cache.class.as_slice(), cache.adjacent_cuts.as_slice()));
        // Everything on the device pixel grid: one solid, continuous shape per rectangle, the
        // same height whatever the width.
        let ppp = ui.ctx().pixels_per_point().max(1.0);
        let px = 1.0 / ppp;
        let snap = |v: f32| (v * ppp).round() / ppp;
        // exactly 2 device pixels of space above and below every indicator, both edges on the
        // device pixel grid (the half-pixel edges rendered as 2 px one side and 3 the other)
        let y0 = ((bar.min.y * ppp).round() + 2.0) / ppp;
        let y1 = (((bar.max.y * ppp).round() - 2.0).max((bar.min.y * ppp).round() + 4.0)) / ppp;
        let fill = app.session.state.timeline_ui.fill_clips;
        let vcol = parse_hex_color(&app.session.state.timeline_ui.video_indicator_color, DEFAULT_VIDEO_COLOR);
        let acol = parse_hex_color(&app.session.state.timeline_ui.audio_indicator_color, DEFAULT_AUDIO_COLOR);
        let ocol = parse_hex_color(&app.session.state.timeline_ui.offline_indicator_color, DEFAULT_OFFLINE_COLOR);
        let color_for = |k: u8| {
            if k == 3 {
                ocol
            } else if (k == 2) != pri_audio {
                vcol
            } else {
                acol
            }
        };
        // 1 pt off each run's ends: the gap between two rectangles is the vertical cut line
        let runs: Vec<(f32, f32, u8)> = minimap_runs(class, adjacent_cuts)
            .into_iter()
            .map(|(start, c, k)| {
                let x0 = snap(bar.min.x + start as f32 + 0.5);
                let x1 = snap(bar.min.x + c as f32 - 0.5).max(x0 + 2.0 * px);
                (x0, x1, k)
            })
            .collect();
        // The bar is a pill: the first clip near the Timeline's start gets a true semicircle on
        // its left -- centered on the band, tangent to the top and bottom edges (a clean D, no
        // lopsided end angles, the same pixels top and bottom). Its leftmost point touches the
        // slider's rounded outline; everywhere else the smaller circle sits inside the pill's
        // own cap, so the indicator can never poke off the track. The right edge is left alone
        // (beyond clamping into the far cap).
        let rad = bar.height() / 2.0;
        let cy = bar.center().y;
        let outline = |x0: f32, x1: f32, inset: f32, hug_left: bool, hug_right: bool| -> Vec<Pos2> {
            let top = y0 + inset;
            let bot = y1 - inset;
            let mut xa = x0 + inset;
            let mut xb = x1 - inset;
            // a cap edge never sits ON the pill's boundary: the outline stroke is half outside
            // its path and the fill antialiases outward -- `inset + px` is what makes the two
            // modes land their edges on the SAME x (the stroke's outer edge == the fill's edge)
            let ins = inset + px;
            let mid = (top + bot) / 2.0;
            // the semicircle sits exactly 2 device pixels in from the slider's edge (the 1 px
            // stroke inset plus this 1 px), the same clearance the runs in the middle keep
            let sh = 1.0 * px;
            let cap_r = (bot - top) / 2.0;
            // the right cap is the left one mirrored exactly: its centre sits the same distance
            // in from the bar's right edge and the curve bends the other way
            let cap_rr = (bot - top) / 2.0;
            (xa, xb) = minimap_indicator_bounds(xa, xb, bar.min.x + ins + sh + cap_r, bar.max.x - ins - sh - cap_rr, hug_left, hug_right);
            let at = |y: f32, left: bool| -> f32 {
                if left {
                    if hug_left {
                        let dy = (y - mid).abs().min(cap_r);
                        bar.min.x + ins + sh + cap_r - (cap_r * cap_r - dy * dy).max(0.0).sqrt()
                    } else {
                        let dy = (y - cy).abs().min(rad);
                        let edge = rad - (rad * rad - dy * dy).max(0.0).sqrt();
                        xa.max(bar.min.x + edge + ins)
                    }
                } else if hug_right {
                    let dy = (y - mid).abs().min(cap_rr);
                    bar.max.x - ins - sh - cap_rr + (cap_rr * cap_rr - dy * dy).max(0.0).sqrt()
                } else {
                    let dy = (y - cy).abs().min(rad);
                    let edge = rad - (rad * rad - dy * dy).max(0.0).sqrt();
                    xb.min(bar.max.x - edge - ins)
                }
            };
            let mut pts = Vec::new();
            pts.push(pos2(at(top, true), top));
            pts.push(pos2(at(top, false), top));
            // every sample sits at the y it is drawn at -- the old half-step skew sampled the
            // curve below the point it drew, leaving the top of the semicircle a pixel further
            // out than the bottom
            let mut y = top;
            while y < bot - 1e-3 {
                y = (y + px).min(bot);
                pts.push(pos2(at(y, false), y));
            }
            pts.push(pos2(at(bot, true), bot));
            let mut y = bot;
            while y > top + 1e-3 {
                y = (y - px).max(top);
                pts.push(pos2(at(y, true), y));
            }
            pts
        };
        // fully opaque, drawn over the slider so nothing ever hides behind it
        for (index, &(x0, x1, k)) in runs.iter().enumerate() {
            // "very near the beginning" / "very near the end": within the pill's cap zones
            let (hug_left, hug_right) = minimap_indicator_hugs_edges(index, runs.len(), x0 - bar.min.x, bar.max.x - x1, rad);
            let col = color_for(k);
            if minimap_indicator_filled(fill, x1 - x0, 5.0 * px, hug_left, hug_right) {
                // solid: one triangle fan over the whole shape (rounded cap included)
                let pts = outline(x0, x1, 0.0, hug_left, hug_right);
                let n = pts.len().max(3);
                let mut cx = 0.0;
                let mut cy2 = 0.0;
                for q in &pts {
                    cx += q.x;
                    cy2 += q.y;
                }
                let mut mesh = egui::Mesh::default();
                mesh.colored_vertex(pos2(cx / n as f32, cy2 / n as f32), col);
                for q in &pts {
                    mesh.colored_vertex(*q, col);
                }
                for i in 0..n as u32 {
                    mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n as u32);
                }
                p.add(egui::Shape::mesh(mesh));
            } else {
                // a 2 device-pixel outline as one continuous closed stroke around the shape
                p.add(egui::Shape::Path(egui::epaint::PathShape::closed_line(outline(x0, x1, px, hug_left, hug_right), Stroke::new(2.0 * px, col))));
            }
            // Keep audio and offline runs distinguishable without relying on their configured
            // colours. These small black-outlined white marks sit inside the indicator, leaving the
            // minimap's colours, outlines, and geometry unchanged.
            if x1 - x0 >= 5.0 * px {
                let mid_x = (x0 + x1) * 0.5;
                let cue = Stroke::new(px, Color32::WHITE);
                let points = match minimap_cue(k, pri_audio) {
                    MinimapCue::None => None,
                    MinimapCue::Audio => Some([pos2(mid_x, y0 + px), pos2(mid_x, y1 - px)]),
                    MinimapCue::Offline => Some([pos2(mid_x - px, y1 - px), pos2(mid_x + px, y0 + px)]),
                };
                if let Some(points) = points {
                    p.line_segment(points, Stroke::new(2.0 * px, Color32::BLACK));
                    p.line_segment(points, cue);
                }
            }
        }
    }
    // Each end circle: a dark 4.5 disc with a white ring AROUND it (4.5..6.0). While a circle is
    // touched or an edge is dragged the ring only comes further IN -- its outer edge never moves --
    // and it stops at the thickness whose dark middle looked right. Touching either circle lights
    // up the other one too.
    let hp = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
    const CIRCLE: f32 = 7.0;
    let cy = thumb.center().y;
    let hi = thumb.height();
    let circle_l = thumb.min.x + hi / 2.0;
    let circle_r = thumb.max.x - hi / 2.0;
    app.auto.add("timeline.zoomBar.handle.left", Rect::from_center_size(pos2(circle_l, cy), vec2(CIRCLE * 2.0, CIRCLE * 2.0)), "Zoom bar left handle");
    app.auto.add("timeline.zoomBar.handle.right", Rect::from_center_size(pos2(circle_r, cy), vec2(CIRCLE * 2.0, CIRCLE * 2.0)), "Zoom bar right handle");
    // while the MIDDLE is grabbed the end circles ignore the pointer entirely (the pinned
    // cursor sits on top of them and used to light their rings up)
    let middle_grab = matches!(app.tl.drag, Some(Drag::ZoomBar { mode: 0, .. }));
    let on_circle = !middle_grab
        && hp.is_some_and(|p| {
            ((p.x - circle_l).abs() <= CIRCLE && (p.y - cy).abs() <= CIRCLE) || ((p.x - circle_r).abs() <= CIRCLE && (p.y - cy).abs() <= CIRCLE)
        });
    let edge_grab = matches!(app.tl.drag, Some(Drag::ZoomBar { mode: 1 | 2, .. }));
    /// The ring's outer edge: a 4.5 disc with a 1.5 ring around it. It never grows outwards.
    const RING_OUT: f32 = 6.0;
    // Hovering an edge (circle or edge zone) brings both rings in 1.5x; clicking or dragging an
    // edge brings them in the full 2.25x. Smooth Animations eases between the states quickly.
    let edge_hover =
        on_circle || (!middle_grab && hp.is_some_and(|p| thumb.expand2(vec2(0.0, 4.0)).contains(p) && (p.x < thumb.min.x + hi || p.x > thumb.max.x - hi)));
    let edge_press = edge_grab || (edge_hover && ui.input(|i| i.pointer.primary_down()));
    let ring_target = if edge_press {
        3.375
    } else if edge_hover {
        2.25
    } else {
        1.5
    };
    let ring_w = if app.session.state.timeline_ui.smooth_animations {
        let k = (ui.input(|i| i.stable_dt).min(0.05) / 0.05).min(1.0);
        let w = app.tl.zoombar_ring_w + (ring_target - app.tl.zoombar_ring_w) * k;
        app.tl.zoombar_ring_w = w;
        if (w - ring_target).abs() > 0.01 {
            ui.ctx().request_repaint();
        }
        w
    } else {
        app.tl.zoombar_ring_w = ring_target;
        ring_target
    };
    for x in [circle_l, circle_r] {
        p.circle_filled(pos2(x, cy), RING_OUT, t.icon_active);
        p.circle_filled(pos2(x, cy), RING_OUT - ring_w, t.panel_bg);
    }
    // The end circles resize the visible range. Show the horizontal resize cursor on the handles
    // only (never the slider's middle), and keep it through the whole edge drag. Shift drags hide
    // the cursor while it is pinned.
    app.tl.zoombar_point_cursor = app.tl.zoombar_cursor_lock.is_none() && (on_circle || edge_grab);
    if app.tl.zoombar_point_cursor {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    // The "you are here" marker on the fixed minimap: the true visible window in the plain linear
    // map, drawn as one blue line (2 pt) with a 1 pt gap to the slider -- above it normally, below
    // it with Alternate Slider Position (where the band sits at the top of the Timeline). The
    // marker is CENTERED on the true window (min width 2 pt, grown symmetrically): under a
    // center-zoom the view's center never moves, so the marker must never drift either -- growing
    // it to the right instead is what made it hop a pixel near the Maximum Zoom Level.
    if app.session.state.timeline_ui.slider_minimap && app.session.state.timeline_ui.show_position_indicator {
        let scroll = if app.ui.timeline.scroll.is_finite() { app.ui.timeline.scroll.max(0.0) } else { 0.0 };
        let vis_s = if vis.is_finite() && vis > 0.0 { vis } else { minimap_span };
        let a = minimap_x(scroll);
        let b = minimap_x(scroll + vis_s);
        // The marker is the visible window on the map: 2 px shorter overall and at least 2 px
        // wide. Each edge tracks its own side of the window -- the left edge never moves when
        // the view sits at the very start of the Timeline and only collapses, and the right edge
        // never moves at the very end -- so zooming at an end cannot make it creep either way.
        let x0 = (a + 1.0).min(bar.max.x - 2.0).max(bar.min.x);
        let x1 = ((b - 1.0).max(x0 + 2.0)).min(bar.max.x);
        // The playhead's EXACT colour (#5895ec): at the default setting the marker is drawn with
        // the theme's own playhead token, so the two can never drift apart (the playhead follows
        // a custom Appearance highlight). A custom setting still wins.
        let marker = if app.session.state.timeline_ui.position_indicator_color.eq_ignore_ascii_case("#5895ec") {
            t.playhead
        } else {
            parse_hex_color(&app.session.state.timeline_ui.position_indicator_color, DEFAULT_POS_COLOR)
        };
        let (my0, my1) =
            if app.session.state.timeline_ui.alternate_slider_position { (bar.max.y + 1.0, bar.max.y + 3.0) } else { (bar.min.y - 3.0, bar.min.y - 1.0) };
        p.rect_filled(Rect::from_min_max(pos2(x0, my0), pos2(x1, my1)), 0.0, marker);
    }
    app.auto.add("timeline.zoomBar", thumb, "zoom scroll bar");
    app.auto.add("timeline.zoomBar.track", bar, "zoom scroll bar track");
    let popup_id = egui::Popup::default_response_id(&resp);
    let anchor_id = popup_id.with("timeline-settings-anchor");
    // a Controls binding the user chose: its chord does what the gesture used to do (the
    // built-in gestures still work). A chord with Ctrl/Alt fires even while a text field has
    // focus (it is a shortcut, not typing); a plain key only when nothing else wants the keyboard.
    let chord = input_text(ui, true);
    let chord_text = chord.as_deref();
    // while a Controls box is naming a new shortcut, no binding may fire on that key. A chord
    // with Ctrl/Alt fires even in a text field (it is a shortcut, not typing); a plain key only
    // when no text field has focus (`egui_wants_keyboard_input` is not enough: any clicked widget
    // counts there, including the box itself).
    let key_pressed = ui.input(|i| {
        let mut normalized = i.events.clone();
        crate::panels::timeline::clipboard_events_as_keys(&mut normalized, i.modifiers, false);
        normalized.iter().any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
    });
    let chord_ok = key_pressed
        && !controls_listening(app, ui.ctx())
        && chord_text.is_some_and(|c| c.contains("Ctrl") || c.contains("Alt") || !ui.ctx().text_edit_focused());
    let (center_bound, explode_bound, alternate_drag_bound, settings_bound) = {
        let s = &app.session.state.timeline_ui;
        let bound = |binding: &str| chord_ok && !binding.is_empty() && chord_text == Some(binding) && chord_text.is_some_and(|c| !c.ends_with("Click"));
        (
            bound(&s.control_center_on_playhead),
            bound(&s.control_explode_slider),
            !controls_listening(app, ui.ctx()) && !s.control_alternate_drag_speed.is_empty() && binding_held(ui, &s.control_alternate_drag_speed),
            bound(&s.control_settings) && ui.rect_contains_pointer(bar),
        )
    };
    // Only key chords can be captured, so they never hijack unrelated pointer gestures. Compare
    // against the stored strings in place: do not clone all four bindings on every UI frame.
    let right_click = ui
        .rect_contains_pointer(bar)
        .then(|| {
            ui.input(|input| {
                input.pointer.button_clicked(egui::PointerButton::Secondary).then(|| input.pointer.interact_pos()).flatten().filter(|pos| bar.contains(*pos))
            })
        })
        .flatten();
    let open_at = right_click.or_else(|| settings_bound.then(|| ui.input(|i| i.pointer.hover_pos().unwrap_or_else(|| bar.center()))));
    let screen = ui.ctx().content_rect();
    let popup_width = 274.0;
    let ctx = ui.ctx().clone();
    let top_anchor_key = popup_id.with("top-anchor");
    if let Some(pos) = open_at {
        let space_above = pos.y - screen.top();
        let space_below = screen.bottom() - pos.y;
        let above = space_above > space_below;
        ui.data_mut(|data| data.insert_temp(anchor_id, (pos, above)));
        ui.data_mut(|data| data.insert_temp(zoombar_popup_open_key(), true));
        ui.data_mut(|data| data.remove::<bool>(top_anchor_key));
        // the menu opens in its default state, never with a flyout or colour picker left over
        zoombar_state_reset(ui.ctx());
        // work on the Timeline's sliders: the keys bound here must fire afterwards
        app.ui.focused = crate::dock::PanelKind::Timeline;
        // the two settings menus are mutually exclusive: this one takes the sliders' with it
        app.tl.track_slider_settings_pos = None;
    }
    if resp.clicked() || resp.drag_started() {
        ui.data_mut(|data| data.insert_temp(zoombar_popup_open_key(), false));
        ui.data_mut(|data| data.remove::<bool>(top_anchor_key));
    }
    let escape_pressed = ui.input(|input| input.key_pressed(egui::Key::Escape));
    let capture_was_active = controls_listening(app, &ctx);
    let popup_was_open = ctx.data(|data| data.get_temp::<bool>(zoombar_popup_open_key())).unwrap_or(false);
    if should_close_zoombar_popup_on_escape(popup_was_open, escape_pressed, capture_was_active) {
        close_zoombar_popup(&ctx);
        zoombar_state_reset(&ctx);
    }
    let (requested_pos, above) =
        ui.data(|data| data.get_temp::<(Pos2, bool)>(anchor_id)).unwrap_or_else(|| (resp.interact_pointer_pos().unwrap_or(bar.min), false));
    let anchor_pos = pos2(
        (if requested_pos.x + popup_width <= screen.right() { requested_pos.x } else { requested_pos.x - popup_width })
            .clamp(screen.left(), (screen.right() - popup_width).max(screen.left())),
        requested_pos.y.clamp(screen.top(), screen.bottom()),
    );
    if open_at.is_some() {
        let auto_count = app.auto.elements.len();
        let measurement_id = popup_id.with("measurement");
        let measurement = egui::Area::new(measurement_id)
            .order(egui::Order::Foreground)
            .default_size(egui::vec2(popup_width, screen.height()))
            .fixed_pos(screen.min)
            .fade_in(false)
            .interactable(false)
            .sizing_pass(true)
            .show(&ctx, |ui| {
                ui.push_id("measurement", |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| zoombar_popup_content(app, ui, screen.height(), true)).response.rect.height()
                })
            });
        let _ = measurement;
        // Egui's sizing pass can produce different widget heights from a normal layout. Run a
        // hidden normal layout too, so the visible area's bottom pivot has its real size immediately.
        let measured =
            egui::Area::new(measurement_id).order(egui::Order::Foreground).fixed_pos(screen.min).fade_in(false).interactable(false).show(&ctx, |ui| {
                ui.set_invisible();
                ui.push_id("measurement", |ui| egui::Frame::popup(ui.style()).show(ui, |ui| zoombar_popup_content(app, ui, screen.height(), true)));
            });
        ctx.data_mut(|data| data.insert_temp(egui::Id::unique("timeline-zoombar-popup-frame-height"), measured.response.rect.height()));
        app.auto.elements.truncate(auto_count);
    }
    let content_height = ctx.data(|data| data.get_temp::<f32>(zoombar_popup_content_height_key())).unwrap_or(0.0);
    let needs_scroll = content_height + 16.0 > screen.height();
    let was_top_anchored = ctx.data(|data| data.get_temp::<bool>(top_anchor_key)).unwrap_or(false);
    let top_anchored = (was_top_anchored || needs_scroll) && (popup_was_open || open_at.is_some());
    if top_anchored {
        ctx.data_mut(|data| data.insert_temp(top_anchor_key, true));
    }
    let mut popup_open = ctx.data(|data| data.get_temp::<bool>(zoombar_popup_open_key())).unwrap_or(false);
    if open_at.is_some() {
        popup_open = true;
    }
    let popup_response = if popup_open {
        let measured_popup_height =
            ctx.data(|data| data.get_temp::<f32>(egui::Id::unique("timeline-zoombar-popup-frame-height"))).unwrap_or(content_height + 16.0);
        // The hidden layout can under-measure the visible popup on the first frame after a
        // minimized viewport is restored. Reserve a little extra frame space so the first
        // painted menu uses its final position instead of jumping on the next repaint.
        let popup_height = (measured_popup_height + 32.0).max(content_height + 16.0).min(screen.height()).max(1.0);
        let (pivot, area_pos) = if top_anchored {
            (egui::Align2::LEFT_TOP, pos2(anchor_pos.x, screen.top()))
        } else if above {
            (egui::Align2::LEFT_BOTTOM, pos2(anchor_pos.x, anchor_pos.y.max(screen.top() + popup_height)))
        } else {
            (egui::Align2::LEFT_TOP, pos2(anchor_pos.x, anchor_pos.y.min(screen.bottom() - popup_height)))
        };
        let panel = egui::Area::new(popup_id)
            .order(egui::Order::Foreground)
            .pivot(pivot)
            .default_size(vec2(popup_width, popup_height))
            .fixed_pos(area_pos)
            .constrain_to(screen)
            .fade_in(true)
            .interactable(true)
            .show(&ctx, |ui| {
                ui.set_min_size(vec2(popup_width, popup_height));
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if content_height + 16.0 > screen.height() {
                        zoombar_popup_content(app, ui, screen.height(), false);
                    } else {
                        zoombar_settings_rows(app, ui);
                    }
                })
            });
        ctx.data_mut(|data| data.insert_temp(egui::Id::unique("timeline-zoombar-popup-rect"), panel.response.rect));
        Some(panel)
    } else {
        ctx.data_mut(|data| data.remove::<egui::Rect>(egui::Id::unique("timeline-zoombar-popup-rect")));
        ctx.data_mut(|data| data.remove::<bool>(top_anchor_key));
        None
    };
    if let Some(click_pos) = ui.input(|input| input.pointer.button_clicked(egui::PointerButton::Primary).then(|| input.pointer.interact_pos()).flatten())
        && popup_response.as_ref().is_some_and(|popup| !popup.response.rect.contains(click_pos))
        // a click on one of the settings' own popups (the Minimap flyout, a colour picker) is not
        // an outside click either -- their pills must not close the menu they belong to
        && !family_contains(ui, click_pos)
    {
        ctx.data_mut(|data| data.insert_temp(zoombar_popup_open_key(), false));
    }
    let range = (total - vis).max(0.0);
    let triple = resp.triple_clicked() || explode_bound;
    let double = resp.double_clicked() || center_bound;
    if triple {
        // triple-click: the whole Timeline in view
        let v = &mut app.ui.timeline;
        // fitting the whole Timeline: the floor is the sequence-dependent one (the same value
        // the fit computes), never a fixed 0.05, or a long sequence can never fit
        let (fit_pps, _) = zoom_pps_limits(seq_seconds, view_w, min_view);
        v.target_pps = fit_pps;
        v.target_scroll = 0.0;
        if !app.session.state.timeline_ui.smooth_animations {
            v.pps = fit_pps;
            v.scroll = 0.0;
        }
        app.tl.zoom_anchor = None;
    } else if double {
        // double-click: the Playhead Centering Range around the playhead
        let side = if app.session.state.timeline_ui.playhead_side_seconds.is_finite() {
            app.session.state.timeline_ui.playhead_side_seconds.clamp(0.1, 3600.0)
        } else {
            10.0
        };
        let span = side * 2.0;
        // fitting the centring range: no fixed 0.05 floor (it stopped a long range from ever
        // fitting), just the Maximum Zoom Level ceiling
        let (floor, ceiling) = zoom_pps_limits(seq_seconds, view_w, min_view);
        let fit_pps = (view_w.max(1.0) as f64 / span.max(1e-9)).clamp(floor, ceiling);
        let playhead = app.session.playhead().seconds();
        let effective_span = view_w.max(1.0) as f64 / fit_pps.max(1e-9);
        let target_scroll = (playhead - effective_span / 2.0).clamp(0.0, (timeline_extent(seq_seconds) - effective_span).max(0.0));
        let v = &mut app.ui.timeline;
        v.target_pps = fit_pps;
        v.target_scroll = target_scroll;
        if !app.session.state.timeline_ui.smooth_animations {
            v.pps = fit_pps;
            v.scroll = target_scroll;
        }
        app.tl.zoom_anchor = None;
    }
    // a press beside the thumb brings the thumb's middle under the pointer (and a drag goes on
    // from there)
    let jump = |v: &mut crate::state::TimelineView, x: f32| {
        if per_point > 0.0 {
            v.target_scroll = ((x - bar.min.x - w / 2.0) as f64 * per_point).clamp(0.0, range);
            v.scroll = v.target_scroll;
        }
    };
    if !double
        && !triple
        && ui.rect_contains_pointer(bar)
        && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
        && let Some(pos) = resp.interact_pointer_pos().filter(|pos| !thumb.contains(*pos))
    {
        // a jump is manual too: it escapes Playhead Centering's hold
        app.tl.playhead_captured = false;
        jump(&mut app.ui.timeline, pos.x);
    }
    // (a drag is known to be one only after the pointer has moved: what was grabbed is what was
    // under the pointer when the button went down)
    if resp.drag_started()
        && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos())
    {
        // Each end circle is a grey 4.5 disc with a white 1.5 ring around it, centred height/2
        // inside the thumb. Grabbing ANY of the circle -- grey or white, inside or outside the
        // thumb -- drags that edge: the circles win over every other interpretation, so a click on
        // the ring can never fall through to "beside the thumb" or to the thumb's middle. The
        // reach is deliberately a little wider than the ring so the smaller target always wins.
        // Distance to the circle centres decides.
        const CIRCLE: f32 = 7.0;
        let hi = thumb.height();
        let cy = thumb.center().y;
        let circle_l = thumb.min.x + hi / 2.0;
        let circle_r = thumb.max.x - hi / 2.0;
        let zone = (hi / 2.0 + 4.5).min((thumb.width() - 1.0) / 2.0).max(1.0);
        let mode = if (pos.x - circle_l).abs() <= CIRCLE && (pos.y - cy).abs() <= CIRCLE {
            1
        } else if (pos.x - circle_r).abs() <= CIRCLE && (pos.y - cy).abs() <= CIRCLE {
            2
        } else if !thumb.expand2(vec2(0.0, 4.0)).contains(pos) {
            // a jump is manual too: it escapes Playhead Centering's hold
            app.tl.playhead_captured = false;
            jump(&mut app.ui.timeline, pos.x);
            0
        } else if pos.x < thumb.min.x + zone {
            1
        } else if pos.x > thumb.max.x - zone {
            2
        } else {
            0
        };
        // hovering an edge circle reads as "click me": point like a link cursor -- only on the
        // circles, never the slider's middle; skipped while dragging (the cursor is pinned)
        if app.tl.drag.is_none() {
            let on_l = (pos.x - circle_l).abs() <= CIRCLE && (pos.y - cy).abs() <= CIRCLE;
            let on_r = (pos.x - circle_r).abs() <= CIRCLE && (pos.y - cy).abs() <= CIRCLE;
            if on_l || on_r {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
        }
        if mode == 0 {
            // The thumb is drawn from the displayed view. A manual grab interrupts any pending
            // centering animation and continues from the position the pointer actually grabbed.
            cancel_scroll_animation_for_manual_drag(&mut app.ui.timeline);
        }
        app.tl.drag = Some(Drag::ZoomBar { grab: pos.x, start: app.ui.timeline.target_scroll, start_pps: app.ui.timeline.target_pps, mode });
        // a fresh edge zoom is a fresh gravity action: re-anchor Playhead Centering on where the
        // playhead sits in the view right now (or let it go when it is off screen -- zooming with
        // the playhead off screen must never snap to it)
        if mode != 0 && app.session.state.timeline_ui.playhead_centering {
            let ph = app.session.playhead().seconds();
            let vis = (view_w.max(1.0) as f64 / app.ui.timeline.pps.max(0.05)).max(1e-6);
            let sc = app.ui.timeline.scroll;
            if ph.is_finite() && ph >= sc && ph <= sc + vis {
                app.tl.playhead_captured = true;
                app.tl.playhead_capture_vis = vis;
                app.tl.playhead_capture_u = ((ph - sc) / vis).clamp(0.0, 1.0);
            } else {
                app.tl.playhead_captured = false;
                app.tl.playhead_capture_vis = 0.0;
                app.tl.playhead_capture_u = 0.5;
            }
        }
    }
    if matches!(app.tl.drag, Some(Drag::ZoomBar { .. }))
        && ui.input(|i| i.key_pressed(egui::Key::Escape))
        && let Some(Drag::ZoomBar { start, start_pps, .. }) = app.tl.drag.take()
    {
        let v = &mut app.ui.timeline;
        v.target_scroll = start;
        v.scroll = start;
        v.target_pps = start_pps;
        v.pps = start_pps;
        // the cancelled drag takes its gravity anchor with it
        app.tl.playhead_captured = false;
        app.tl.playhead_capture_vis = 0.0;
        app.tl.playhead_capture_u = 0.5;
    }
    if let Some(Drag::ZoomBar { mode, .. }) = app.tl.drag
        && resp.dragged()
    {
        app.timeline_auto_scroll_pending = None;
        let shift = ui.input(|i| i.modifiers.shift) || alternate_drag_bound;
        // While Shift is held in a zoom-bar drag the OS cursor is made invisible with a fully
        // transparent 1x1 cursor bitmap, confined to the app window, and PINNED to the exact
        // position where Shift went down: every frame it is warped back there, so it never moves
        // while Shift is held, never hits a screen or window edge, and reappears on release at the
        // very pixel where it vanished. The invisible cursor plus confinement exist too -- but
        // winit's own hide must NOT be used, which clips a grabbed + hidden cursor to a one-pixel
        // rect at the window's center on Windows (winit window_state.rs refresh_os_cursor) and
        // freezes it. A transparent bitmap avoids that HIDDEN flag entirely, so confinement works.
        //
        // While pinned, the pointer's frame delta is measured by hand (current position minus the
        // pin position) instead of `drag_delta()`, because the per-frame warps confuse egui's
        // pointer position history.
        let mut shift_dx = 0.0f64;
        if shift && let Some(origin) = ui.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos())) {
            if app.tl.zoombar_cursor_lock.is_none() {
                let viewport = ui.ctx().viewport_id();
                app.tl.zoombar_cursor_lock = Some((viewport, origin));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::viewport::CursorGrab::Confined));
            }
            let pinned = app.tl.zoombar_cursor_lock.map(|(_, position)| position).unwrap_or(origin);
            let now = ui.input(|i| i.pointer.latest_pos().unwrap_or(pinned));
            shift_dx = f64::from(now.x - pinned.x);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorPosition(pinned));
            ui.ctx().set_cursor_image(Some(transparent_cursor()));
        } else if let Some((viewport, _origin)) = app.tl.zoombar_cursor_lock.take() {
            app.tl.zoombar_unlock_catchup = true;
            release_zoombar_cursor(ui.ctx(), viewport);
        }
        // While pinned the hand-measured delta is exact; otherwise plain egui pointer motion,
        // exactly since the last frame. Never retroactive: pressing and letting go of Shift mid-drag
        // just changes the speed and never re-moves what was already dragged --
        // `zoombar_unlock_catchup` skips the one unlock frame whose warp would otherwise show up as
        // a bogus negative delta.
        let plain_dx = resp.drag_delta().x;
        let moved = if app.tl.zoombar_cursor_lock.is_some() {
            shift_dx
        } else if plain_dx.is_finite() && !app.tl.zoombar_unlock_catchup {
            plain_dx as f64
        } else {
            0.0
        };
        if app.tl.zoombar_unlock_catchup {
            // one frame of no movement: the final warp back to the pin position settles here
            app.tl.zoombar_unlock_catchup = false;
        }
        // The Alternate Drag Speed setting scales Shift's sensitivity above/below the tuned curve
        // for the view's current zoom (see [`shift_drag_base`]). Without Shift the drag is exactly
        // 1:1 -- the thumb moves with the mouse, one point moves it one point.
        let base = if shift && app.tl.zoombar_cursor_lock.is_some() {
            let full_view = timeline_extent(seq_seconds) * view_w.max(1.0) as f64 / f64::from(bar.width().max(1.0));
            let pps = app.ui.timeline.pps;
            let visible = if pps.is_finite() && pps > 0.0 { view_w.max(1.0) as f64 / pps } else { full_view };
            let speed =
                if app.session.state.timeline_ui.shift_drag_speed.is_finite() { app.session.state.timeline_ui.shift_drag_speed.clamp(0.1, 50.0) } else { 1.0 };
            shift_drag_base(visible, full_view, frame_seconds) * speed
        } else {
            1.0
        };
        let dx = moved * base;
        let v = &mut app.ui.timeline;
        match mode {
            // the thumb's middle: scroll, the thumb staying under the pointer. Dragging the slider
            // by hand is the one way out of Playhead Centering's hold ("event horizon") -- it
            // releases the capture until a zoom catches the playhead in view again.
            0 => {
                if moved != 0.0 && app.tl.playhead_captured {
                    app.tl.playhead_captured = false;
                }
                v.target_scroll = (v.target_scroll + dx * per_point).clamp(0.0, range);
                v.scroll = v.target_scroll;
            }
            // either edge handle: the whole slider is mirrored about its middle -- dragging an
            // edge outward grows both edges equally (the view sees less), dragging it inward
            // shrinks both. The footage at the view's center stays put.
            _ => {
                // 1:1 with the mirrored thumb in the LOG-mapped range: dragging an edge `dx` points
                // also moves the other edge `dx` points, so the thumb grows by `2 * dx` points of
                // bar width -- the zoom changes however much that share of the log range means. The
                // mirror runs the same direction on both edges; for the LEFT edge the drag
                // direction flips the meaning, hence the sign.
                let dir = if mode == 1 { -1.0 } else { 1.0 };
                let vis = view_w.max(1.0) as f64 / v.pps.max(1e-9);
                // Alternate Drag Speed adjusts edge grabs only -- the middle is never touched by
                // the extra zoomed-in sensitivity ([`shift_edge_speed_multiplier`]).
                let dx = if shift && app.tl.zoombar_cursor_lock.is_some() { dx * shift_edge_speed_multiplier(vis) } else { dx };
                let f0 = log_frac(vis, total, min_view);
                // the thumb is MIN_THUMB_W + f * (bar - MIN_THUMB_W) wide, so this keeps the
                // growth at exactly 2 points of thumb per point of drag
                let nf = (f0 + dir * dx / f64::from((bar.width() - MIN_THUMB_W).max(1.0)) * 2.0).clamp(0.0, 1.0);
                let new_vis = vis_at_frac(nf, total, min_view);
                // Playhead Centering works off the view as it was BEFORE this frame's zoom: with
                // the playhead off screen the zoom is plain zooming and nothing ever snaps to it.
                let ph = app.session.playhead().seconds();
                let was_in_view = ph >= v.scroll && ph <= v.scroll + vis;
                zoom_about_center(v, view_w, seq_seconds, total, view_w.max(1.0) as f64 / new_vis, min_view);
                v.target_scroll = v.scroll;
                app.tl.zoom_anchor = None;
                // Playhead Centering: while a zoom drags the playhead into the visible range the
                // view is caught ("gravity"). ONE smooth action afterwards: where the playhead sits
                // in the view eases from where it was caught toward the middle as the view
                // collapses (smoothstep, so it neither starts nor ends with a jolt, fully centered
                // once the visible range has roughly halved). The pull is a function of the zoom
                // itself, so it can never stall at the edge and then snap in a few frames. Once
                // caught the playhead never leaves the screen. Grabbing the slider's middle pulls
                // it off manually (mode 0).
                if app.session.state.timeline_ui.playhead_centering {
                    let vis = view_w.max(1.0) as f64 / v.pps.max(1e-9);
                    let range = (total - vis).max(0.0);
                    // a capture left over from an earlier gesture whose playhead some other edit
                    // has since moved off screen is stale: drop it rather than ever yank the view
                    // across the timeline to the playhead
                    if app.tl.playhead_captured && !was_in_view {
                        app.tl.playhead_captured = false;
                        app.tl.playhead_capture_vis = 0.0;
                        app.tl.playhead_capture_u = 0.5;
                    }
                    if !app.tl.playhead_captured && was_in_view {
                        app.tl.playhead_captured = true;
                        app.tl.playhead_capture_vis = vis;
                        app.tl.playhead_capture_u = ((ph - v.scroll) / vis.max(1e-9)).clamp(0.0, 1.0);
                    }
                    if app.tl.playhead_captured {
                        let vis0 = if app.tl.playhead_capture_vis.is_finite() && app.tl.playhead_capture_vis > 0.0 { app.tl.playhead_capture_vis } else { vis };
                        let u0 = if app.tl.playhead_capture_u.is_finite() { app.tl.playhead_capture_u.clamp(0.0, 1.0) } else { 0.5 };
                        let s = ((1.0 - vis / vis0) * 2.0).clamp(0.0, 1.0);
                        let e = s * s * (3.0 - 2.0 * s);
                        let u = u0 + (0.5 - u0) * e;
                        // the hard floor under the smooth pull: the scroll that keeps the playhead
                        // inside the visible range (ph - vis ..= ph), within the timeline
                        let lo = (ph - vis).max(0.0).min(range);
                        let hi = ph.max(0.0).min(range);
                        v.scroll = (ph - u * vis).max(lo).min(hi);
                        v.target_scroll = v.scroll;
                    }
                }
            }
        }
        // the wheel while grabbing the middle: zoom evenly about the view's center -- both edges of
        // the thumb move toward or away from each other symmetrically, no matter where within the
        // middle was grabbed. Wheel up zooms IN (the thumb shrinks), wheel down zooms out. Shift
        // slows the wheel down (the Alternate Drag Speed setting on top of a tenth of the ordinary
        // effect: 1x is ten times slower, Custom 50x five times faster) and counts a sideways
        // gesture too -- Shift + wheel reports as horizontal scroll on some platforms.
        if mode == 0 {
            let shift = ui.input(|i| i.modifiers.shift);
            let (wy, wx) = ui.input(|i| (i.smooth_scroll_delta.y, i.smooth_scroll_delta.x));
            let wheel = if shift { wy + wx } else { wy };
            let mut factor = (wheel as f64 / 600.0).clamp(-0.30, 0.30);
            if shift {
                let speed = if app.session.state.timeline_ui.shift_drag_speed.is_finite() {
                    app.session.state.timeline_ui.shift_drag_speed.clamp(0.1, 50.0)
                } else {
                    1.0
                };
                factor = (factor * speed / 10.0).clamp(-0.9, 0.9);
            }
            // Inverted controls: the wheel zooms the other way here too
            if app.session.state.timeline_ui.inverted_controls {
                factor = -factor;
            }
            if wheel.is_finite() && factor != 0.0 {
                let v = &mut app.ui.timeline;
                zoom_about_center(v, view_w, seq_seconds, total, v.pps * (1.0 + factor), min_view);
                app.tl.zoom_anchor = None;
            }
        }
    }
    if resp.drag_stopped() {
        app.tl.drag = None;
        app.tl.zoombar_unlock_catchup = false;
    }
}

/// A 1x1 fully transparent cursor bitmap: hides the OS cursor without using winit's hidden flag
/// (which would clip a confined cursor to the window's center on Windows). `Arc<[u8]>` lets the
/// integration dedupe, but the 4-byte allocation per frame is trivial anyway.
fn transparent_cursor() -> egui::CustomCursorImage {
    egui::CustomCursorImage { rgba: [0, 0, 0, 0].into(), size: [1, 1], hotspot: [0, 0] }
}

/// Release the invisible, confined zoom-bar drag cursor: restore the standard cursor and let the
/// window edges go. Nothing warps -- the cursor is simply wherever the user's hand left it.
fn release_zoombar_cursor(ctx: &egui::Context, viewport: egui::ViewportId) {
    ctx.set_cursor_image(None);
    ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::CursorGrab(egui::viewport::CursorGrab::None));
}

/// Close the zoom bar's own settings popup (the one opened by right-clicking the bar). The two
/// sliders' settings menus are mutually exclusive: opening one takes the other with it.
pub(super) fn close_zoombar_popup(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(zoombar_popup_open_key(), false));
}

/// The window that comes up when a slider is hidden: it only tells the user how to bring it
/// back, OK dismisses it. Drawn from the Timeline itself rather than the bar, so it still shows
/// once the slider is gone. One for the zoom scroll bar, one for the side track sliders.
pub fn zoombar_hide_dialog(app: &mut FilmcraftApp, ctx: &egui::Context) {
    hide_dialog(app, ctx, false);
    hide_dialog(app, ctx, true);
}

fn hide_dialog(app: &mut FilmcraftApp, ctx: &egui::Context, track: bool) {
    let open = if track { app.tl.track_hide_dialog_open } else { app.tl.zoombar_hide_dialog_open };
    if !open {
        return;
    }
    let row = if track { tl!("Unhide Track Slider") } else { tl!("Unhide Timeline Slider") };
    let mut dismiss = false;
    let mut restore = false;
    egui::Window::new(tl!("Hide Slider"))
        .id(egui::Id::unique(("timeline-zoombar-hide-dialog", track)))
        .collapsible(false)
        .resizable(false)
        .fixed_size(egui::vec2(440.0, 0.0))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(tlf!("You can bring it back anytime by right clicking the timeline and selecting \"{row}\" at the bottom of the list.", row = row));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let unhide = ui.button(row);
                let id = if track { "timeline.trackSlider.hideDialog.restore" } else { "timeline.zoomBar.hideDialog.restore" };
                app.auto.add(id, unhide.rect, "Restore the hidden Timeline slider");
                restore = unhide.clicked();
                let ok = ui.button(tl!("OK"));
                let ok_id = if track { "timeline.trackSlider.hideDialog.ok" } else { "timeline.zoomBar.hideDialog.ok" };
                app.auto.add(ok_id, ok.rect, "Close the dialog");
                if ok.clicked() {
                    dismiss = true;
                }
            });
        });
    if restore {
        if track {
            set_track_slider_hidden(app, ctx, false);
        } else {
            set_zoombar_hidden(app, ctx, false);
        }
        return;
    }
    if dismiss {
        if track {
            app.tl.track_hide_dialog_open = false;
        } else {
            app.tl.zoombar_hide_dialog_open = false;
        }
        ctx.request_repaint();
    }
}

/// Hide or bring back the zoom scroll bar (Hide Slider / Unhide Timeline Slider). Hiding unwinds
/// everything the bar keeps running -- a Shift drag mid-hide would leave the cursor pinned and
/// invisible -- so nothing of it is left for later frames to tick over until it is brought back.
pub fn set_zoombar_hidden(app: &mut FilmcraftApp, ctx: &egui::Context, hidden: bool) {
    if app.session.state.timeline_ui.zoom_bar_hidden == hidden {
        return;
    }
    app.session.state.timeline_ui.zoom_bar_hidden = hidden;
    app.session.project_view_changed();
    if hidden {
        close_zoombar_popup(ctx);
        app.tl.zoombar_settings_pos = None;
        app.tl.zoombar_hide_dialog_open = false;
        ctx.data_mut(|data| {
            data.remove::<f32>(zoombar_popup_content_height_key());
            data.insert_temp(controls_listen_key(), None::<usize>);
        });
        if matches!(app.tl.drag, Some(Drag::ZoomBar { .. })) {
            app.tl.drag = None;
        }
        app.tl.zoom_anchor = None;
        let cursor_viewport = app.tl.zoombar_cursor_lock.take().map(|(viewport, _)| viewport).unwrap_or_else(|| ctx.viewport_id());
        app.tl.zoombar_unlock_catchup = false;
        app.tl.zoombar_point_cursor = false;
        release_zoombar_cursor(ctx, cursor_viewport);
    } else {
        // a stale "how to bring it back" window means nothing once the bar is back
        app.tl.zoombar_hide_dialog_open = false;
    }
}

/// A global safety net for the invisible zoom-bar drag cursor: the lock only exists while a
/// zoom-bar drag is really going on under Shift with the window focused and the primary button
/// down. Every frame this unwinds anything that slipped through (a lag spike, a lost focus, a
/// changed panel, Escape mid-press), so the cursor can never stay invisible or confined when it
/// shouldn't.
pub fn zoombar_cursor_watchdog(tl: &mut super::TlState, ctx: &egui::Context, alternate_speed_bindings: &[&str]) {
    // the pin is legitimate during a zoom-bar Shift drag or a track slider's (which pins the
    // same cursor to its own press pixel)
    let live_drag = matches!(tl.drag, Some(Drag::ZoomBar { .. })) || tl.track_slider_dragging;
    let still = tl.zoombar_cursor_lock.is_some_and(|(viewport, _)| {
        live_drag
            && ctx.input_for(viewport, |i| {
                let alternate_speed = alternate_speed_bindings.iter().filter(|binding| !binding.is_empty()).any(|binding| {
                    i.keys_down.iter().any(|key| {
                        binding_matches_held_key(binding, &format!("{key:?}"), i.modifiers.ctrl || i.modifiers.mac_cmd, i.modifiers.shift, i.modifiers.alt)
                    })
                });
                i.focused && i.pointer.button_down(egui::PointerButton::Primary) && (i.modifiers.shift || alternate_speed)
            })
    });
    if !still && let Some((viewport, _origin)) = tl.zoombar_cursor_lock.take() {
        tl.zoombar_unlock_catchup = true;
        release_zoombar_cursor(ctx, viewport);
    }
}

#[cfg(test)]
mod cursor_viewport_watchdog_tests {
    use super::zoombar_cursor_watchdog;
    use crate::panels::timeline::TlState;
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput, ViewportId};

    #[test]
    fn watchdog_checks_and_releases_the_viewport_that_owns_the_cursor_lock() {
        let ctx = Context::default();
        let pos = Pos2::new(20.0, 30.0);
        let modifiers = Modifiers { shift: true, ..Modifiers::NONE };
        let input = RawInput {
            focused: true,
            events: vec![Event::ModifiersChanged(modifiers), Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers }],
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(input, |_| {});
        output.textures_delta.clear();

        let mut tl = TlState { zoombar_cursor_lock: Some((ViewportId::ROOT, pos)), track_slider_dragging: true, ..TlState::default() };
        zoombar_cursor_watchdog(&mut tl, &ctx, &[]);
        assert!(tl.zoombar_cursor_lock.is_some(), "the focused owner still holds Shift and the primary button");

        let floating = ViewportId::from_hash_of("floating timeline");
        tl.zoombar_cursor_lock = Some((floating, pos));
        zoombar_cursor_watchdog(&mut tl, &ctx, &[]);
        assert!(tl.zoombar_cursor_lock.is_none(), "root-window input cannot keep a floating viewport's cursor captured");
    }

    #[test]
    fn watchdog_keeps_cursor_lock_for_a_held_alternate_speed_chord() {
        let ctx = Context::default();
        let pos = Pos2::new(20.0, 30.0);
        let input = RawInput {
            focused: true,
            events: vec![
                Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                Event::Key { key: egui::Key::K, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            ],
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(input, |_| {});
        output.textures_delta.clear();
        let mut tl = TlState { zoombar_cursor_lock: Some((ViewportId::ROOT, pos)), track_slider_dragging: true, ..TlState::default() };

        zoombar_cursor_watchdog(&mut tl, &ctx, &["K"]);

        assert!(tl.zoombar_cursor_lock.is_some(), "the configured alternate-speed chord keeps the drag cursor pinned");
    }
}

/// The side (track) sliders: the zoom bar's exact look turned on its end -- a rounded pill
/// track, a pill thumb, and the two ringed end circles (a dark disc with a white ring around it;
/// hovering an end brings the rings in 1.5x, pressing or dragging 2.25x). The middle drags the
/// scroll 1:1, a click beside the thumb jumps there, and dragging an end circle (or the edge
/// zone under it) zooms the tracks' height: an outward pull on EITHER end (top up, bottom down)
/// grows the thumb (shorter tracks), an inward pull shrinks it, always about the view's CENTER.
/// Double-click copies
/// the thumb's length and position to the other slider, Shift+double-click sets the minimum track height.
/// Shift drags run at a fraction of the ordinary speed (the Alternate Drag Speed setting's own
/// rate -- the same rate on both sliders). Right-click opens the settings popup, to the left of the sliders. Returns
/// (pointer on an end circle, the thumb's rect for automation ids, 1 = double-click,
/// 2 = Shift+double-click).
// one parameter per piece of slider state keeps the two sliders (video/audio) symmetric at
// their call sites; the drag logic is one state machine like the zoom bar's
#[allow(clippy::type_complexity, clippy::too_many_arguments, clippy::cognitive_complexity)]
pub(super) fn vertical_scrollbar(
    ui: &mut egui::Ui,
    tl: &mut super::TlState,
    bar: Rect,
    value: &mut f32,
    max: f32,
    window_h: f32,
    // how many track slots the content has (the rows plus the empty slot for a new track): the
    // thumb and the zoom maths need it, and it cannot be recovered from `max` (which is 0
    // whenever everything fits)
    slots: f32,
    track_h: &mut f32,
    invert: bool,
    smooth: bool,
    inverted: bool,
    decouple: bool,
    shift_speed: f32,
    t: &Tokens,
    id: &str,
    bindings: [&str; 4],
) -> (bool, Rect, u8) {
    if bar.height() < 2.0 || bar.width() < 6.0 {
        return (false, bar, 0);
    }
    /// The thumb's shortest length: the exact same minimum as the zoom bar's thumb
    /// ([`MIN_THUMB_W`]); the two end circles fit inside it.
    const THUMB_MIN: f32 = MIN_THUMB_W;
    let p = ui.painter();
    p.rect_filled(bar, bar.width() / 2.0, t.separator);
    // with nothing to scroll the thumb fills the track -- the slider is always there, it never
    // just disappears at the ends of the A/V divider's range
    // the thumb is the track-height meter, unbounded: 22 pt tracks fill the bar (full zoom out)
    // and 220 pt leave the MIN_THUMB_W minimum. (A cap here once kept the scroll a travel to use,
    // but it also made full zoom-out unreachable and the drag fought the cap and vibrated.)
    let h = track_thumb(*track_h, bar.height()).max((bar.width() + 4.0).min(bar.height())).min(bar.height());
    let travel = (bar.height() - h).max(1.0);
    // with nothing to scroll the thumb sits in the MIDDLE of the track: pinned to an end it
    // could never collapse about its middle (the dragged edge then barely moved while the other
    // one swung the whole way)
    let pos = if max > 0.5 { (if invert { 1.0 - *value / max } else { *value / max }).clamp(0.0, 1.0) } else { 0.5 };
    let y = bar.min.y + travel * pos;
    let thumb = Rect::from_min_size(pos2(bar.min.x, y), vec2(bar.width(), h));
    let resp = ui.interact(bar, egui::Id::unique(("tl-vs", id)), Sense::click_and_drag());
    let chord = input_text(ui, true);
    let chord_text = chord.as_deref();
    let chord_ok = !controls_listening_for_bar(ui.ctx())
        && chord_text.is_some_and(|value| value.contains("Ctrl") || value.contains("Alt") || !ui.ctx().text_edit_focused());
    let over_bar = ui.rect_contains_pointer(bar);
    let copy_binding = over_bar && chord_ok && !bindings[0].is_empty() && chord_text == Some(bindings[0]);
    let minimum_binding = over_bar && chord_ok && !bindings[1].is_empty() && chord_text == Some(bindings[1]);
    let alternate_binding = !controls_listening_for_bar(ui.ctx()) && !bindings[2].is_empty() && binding_held(ui, bindings[2]);
    let settings_binding = over_bar && chord_ok && !bindings[3].is_empty() && chord_text == Some(bindings[3]);
    let hover = ui.rect_contains_pointer(thumb) || resp.dragged();
    p.rect_filled(thumb, thumb.width() / 2.0, if hover { t.scroll_thumb_hover } else { t.scroll_thumb });
    const CIRCLE: f32 = 7.0;
    const RING_OUT: f32 = 6.0;
    const ZONE: f32 = 12.0;
    let cx = thumb.center().x;
    let wi = thumb.width();
    let circle_t = thumb.min.y + wi / 2.0;
    let circle_b = thumb.max.y - wi / 2.0;
    // what the press grabbed (end circle / edge = zoom, middle = scroll), remembered for the drag
    let mode_key = egui::Id::unique(("tl-vs-mode", id));
    if resp.drag_started() {
        let mode = if let Some(q) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
            if (q.y - circle_t).abs() <= CIRCLE && (q.x - cx).abs() <= CIRCLE {
                1u8
            } else if (q.y - circle_b).abs() <= CIRCLE && (q.x - cx).abs() <= CIRCLE {
                2u8
            } else if q.y < thumb.min.y || q.y > thumb.max.y {
                0u8
            } else if q.y < thumb.min.y + ZONE {
                1u8
            } else if q.y > thumb.max.y - ZONE {
                2u8
            } else {
                0u8
            }
        } else {
            0u8
        };
        ui.data_mut(|d| d.insert_temp(mode_key, mode));
    }
    let mode: u8 = ui.data(|d| d.get_temp(mode_key)).unwrap_or(0);
    if resp.drag_stopped() {
        ui.data_mut(|d| d.remove_temp::<u8>(mode_key));
    }
    let hp = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
    // while the MIDDLE is dragged the end circles ignore the pointer entirely (the pinned
    // cursor sits on top of them and used to light their rings up)
    let middle_drag = resp.dragged() && mode == 0;
    let on_circle = !middle_drag
        && hp.is_some_and(|q| {
            ((q.y - circle_t).abs() <= CIRCLE && (q.x - cx).abs() <= CIRCLE) || ((q.y - circle_b).abs() <= CIRCLE && (q.x - cx).abs() <= CIRCLE)
        });
    let ring_target = if middle_drag {
        1.5
    } else if resp.dragged() && (mode == 1 || mode == 2) || (on_circle && ui.input(|i| i.pointer.primary_down())) {
        3.375
    } else if on_circle || hp.is_some_and(|q| thumb.expand2(vec2(4.0, 0.0)).contains(q) && (q.y < thumb.min.y + wi || q.y > thumb.max.y - wi)) {
        2.25
    } else {
        1.5
    };
    // Smooth Animations (this slider's own) eases between the ring states exactly as the zoom
    // bar does; each slider keeps its own eased width so the two can never fight over one
    let ri = if id == "vs" { 0 } else { 1 };
    let ring_w = if smooth {
        let k = (ui.input(|i| i.stable_dt).min(0.05) / 0.05).min(1.0);
        let w = tl.track_ring_w[ri] + (ring_target - tl.track_ring_w[ri]) * k;
        tl.track_ring_w[ri] = w;
        if (w - ring_target).abs() > 0.01 {
            ui.ctx().request_repaint();
        }
        w
    } else {
        tl.track_ring_w[ri] = ring_target;
        ring_target
    };
    for cy2 in [circle_t, circle_b] {
        p.circle_filled(pos2(cx, cy2), RING_OUT, t.icon_active);
        p.circle_filled(pos2(cx, cy2), RING_OUT - ring_w, t.panel_bg);
    }
    // an end circle reads as "click me": a link-style pointing cursor there (the caller ORs the
    // answer into the zoom bar's flag, which wins over the tracks' grab cursors later in the
    // frame). It stays a pointer for the WHOLE edge drag -- the slider can run away from the
    // cursor and the middle is never affected
    let point = on_circle || (resp.dragged() && matches!(mode, 1 | 2));
    if point {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if resp.dragged() {
        tl.track_slider_dragging = true;
        // a drag writes straight through: no Smooth Animations target may fight it
        tl.track_anim = None;
        // Shift+drag pins the cursor to its press pixel and hides it -- the zoom bar's proven
        // recipe, adapted to the vertical axis: a fully transparent 1x1 cursor image instead of
        // winit's hidden flag (which clips a grabbed + hidden cursor to the window's center), a
        // Confined grab, and a warp back to the pin every frame; the motion is measured by hand
        // against the pin. Without Shift the drag is ordinary pointer motion.
        let shift = ui.input(|i| i.modifiers.shift);
        let alternate_speed = shift || alternate_binding;
        let mut pinned_dy = 0.0f64;
        if alternate_speed && let Some(origin) = ui.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos())) {
            if tl.zoombar_cursor_lock.is_none() {
                let viewport = ui.ctx().viewport_id();
                tl.zoombar_cursor_lock = Some((viewport, origin));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::viewport::CursorGrab::Confined));
            }
            let pinned = tl.zoombar_cursor_lock.map(|(_, position)| position).unwrap_or(origin);
            let now = ui.input(|i| i.pointer.latest_pos().unwrap_or(pinned));
            pinned_dy = f64::from(now.y - pinned.y);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorPosition(pinned));
            ui.ctx().set_cursor_image(Some(transparent_cursor()));
        } else if tl.zoombar_cursor_lock.is_some() && !matches!(tl.drag, Some(Drag::ZoomBar { .. })) {
            tl.zoombar_unlock_catchup = true;
            if let Some((viewport, _)) = tl.zoombar_cursor_lock.take() {
                release_zoombar_cursor(ui.ctx(), viewport);
            }
        }
        let plain_dy = resp.drag_delta().y;
        let moved = if tl.zoombar_cursor_lock.is_some() {
            pinned_dy
        } else if plain_dy.is_finite() && !tl.zoombar_unlock_catchup {
            f64::from(plain_dy)
        } else {
            0.0
        };
        if tl.zoombar_unlock_catchup {
            // one frame of no movement: the final warp back to the pin settles here
            tl.zoombar_unlock_catchup = false;
        }
        // Alternate Drag Speed (Shift drags): the setting SQUARED on a FIXED rate per point of
        // pointer motion -- the same rate on both sliders whatever their track counts, so a 12
        // track video slider and a 3 track audio slider feel 1:1 with each other. 1x is slow,
        // Custom 10x is a hundred times it (many times faster than the plain drag).
        let shift_on = (shift || alternate_binding) && tl.zoombar_cursor_lock.is_some();
        let rate = shift_drag_rate(shift_speed);
        // the height zoom moves the THUMB (0.06 thumb points per pointer point at 1x), the scroll
        // moves the CONTENT (0.2 content points): each rate is a constant, never one the track
        // count or the scroll range can skew
        let dy = (moved * if shift_on { rate * SHIFT_THUMB_RATE } else { 1.0 }) as f32;
        match mode {
            // an end circle or the edge zone under it: zoom the tracks' height. The dragged edge
            // follows the mouse 1:1 and the other edge mirrors it, so the thumb collapses/grows
            // about its own middle (never toward the dragged circle). The motion is linear in the
            // THUMB's length -- linear in the track height felt exponential -- and hard-stops at
            // the ends of the range: once the thumb is at its smallest the drag does nothing at
            // all (no drift, no scroll, nothing).
            1 | 2 => {
                let bar_h = bar.height();
                let slots = slots.max(1.0);
                // The DRAWN thumb is the source of truth and the height is its log-meter
                // inverse ([`track_height_of`]): a bijection, so the drag can neither explode
                // nor compound over its frames.
                let height_of = |len: f32| track_height_of(len, bar_h);
                // a short (but non-collapsed) Timeline can leave the bar under the 34 pt thumb
                // minimum: every clamp must keep its bounds in order or it panics
                let lo = THUMB_MIN.min(bar_h);
                let hi = bar_h;
                // With nothing to scroll the thumb is redrawn at the middle of the bar every
                // frame: a decoupled "the other edge stays put" model cannot survive that, so
                // the drag uses the mirrored-middle geometry there
                let edge = decouple && max > 0.5;
                // geometry is in drawn-thumb points (the thumb cannot leave its track)
                let top_of = |len: f32| {
                    let len = len.clamp(lo, hi);
                    if edge { if mode == 1 { y + h - len } else { y } } else { y + h / 2.0 - len / 2.0 }
                };
                let out = if mode == 1 { -dy } else { dy };
                // a bar too short for the minimum thumb cannot host the height zoom at all:
                // with `lo == hi` every drag frame would clamp to the bar and snap the tracks
                // to their tallest
                if hi > lo {
                    let new_len = (h + if edge { out } else { 2.0 * out }).clamp(lo, hi);
                    if (new_len - h).abs() > f32::EPSILON {
                        let len = new_len;
                        let new_h = height_of(len);
                        let max_new = (slots * new_h - window_h).max(0.0);
                        if decouple {
                            let top = top_of(len);
                            let travel2 = (bar_h - len.clamp(lo, hi)).max(1.0);
                            let p2 = ((top - bar.min.y) / travel2).clamp(0.0, 1.0);
                            *value = if invert { (1.0 - p2) * max_new } else { p2 * max_new };
                        } else {
                            *value = centered_track_scroll(*value, window_h, *track_h, new_h, max_new);
                        }
                        *track_h = new_h;
                    }
                }
            }
            // the middle: 1:1 scroll, as the zoom bar's middle (Inverted controls never touches
            // it). The wheel while the middle is held zooms the tracks' height about the thumb's
            // middle, like the end circles -- the other way with Inverted controls. Shift slows
            // the wheel down (the Alternate Drag Speed setting on top of a tenth of the ordinary
            // effect) and counts a sideways gesture too. The wheel and the drag run together:
            // while the middle is dragged the scroll keeps its fraction of the new range, so the
            // slider never freezes for the length of a wheel gesture.
            _ => {
                if max > 0.5 {
                    // plain: the thumb follows the pointer 1:1; Shift: the content moves at the
                    // fixed Shift rate above. A thumb that fills the bar (22 pt tracks) leaves no
                    // travel to follow, so the scroll per point is floored -- one pixel can never
                    // sweep the whole range again.
                    let d = if shift_on { (moved * rate * SHIFT_CONTENT_RATE) as f32 } else { dy / travel.max(24.0) * max };
                    *value = (*value + if invert { -d } else { d }).clamp(0.0, max);
                }
                let shift = ui.input(|i| i.modifiers.shift);
                let (wy, wx) = ui.input(|i| (i.smooth_scroll_delta.y, i.smooth_scroll_delta.x));
                let mut wheel = if shift { wy + wx } else { wy };
                if shift {
                    // Custom 10x is a real speed-up (2.5x the plain wheel), not a wash
                    wheel *= shift_speed.max(0.01) / 4.0;
                }
                if inverted {
                    wheel = -wheel;
                }
                let factor = (wheel / 600.0).clamp(-0.30, 0.30);
                if wheel.is_finite() && factor != 0.0 {
                    let bar_h = bar.height();
                    let slots = slots.max(1.0);
                    let new_h = (*track_h * (1.0 + factor)).clamp(super::MIN_TRACK_HEIGHT, super::MAX_TRACK_HEIGHT);
                    let glen = track_thumb(new_h, bar_h).clamp(THUMB_MIN.min(bar_h), bar_h);
                    let max_new = (slots * new_h - window_h).max(0.0);
                    // a drag writes the scroll this frame already: keep its fraction of the new
                    // range (the thumb's position fraction, the same `p2` the drawn geometry
                    // gave). Deriving it from the drawn thumb instead threw every frame of the
                    // drag away and froze the slider until the wheel gesture ended.
                    let frac = if resp.dragged() {
                        if max > 0.5 { (if invert { 1.0 - *value / max } else { *value / max }).clamp(0.0, 1.0) } else { 0.5 }
                    } else {
                        let mid_y = y + h / 2.0;
                        let travel2 = (bar_h - glen).max(1.0);
                        ((mid_y - glen / 2.0 - bar.min.y) / travel2).clamp(0.0, 1.0)
                    };
                    *value = if invert { (1.0 - frac) * max_new } else { frac * max_new };
                    *track_h = new_h;
                    tl.track_anim = None;
                }
            }
        }
    }
    // double-click copies this thumb's length and position to the other slider; Shift+double-
    // click sets the minimum track height; Ctrl+Shift+double-click does so for BOTH sliders. Either way the single
    // click beneath them is not a jump.
    let clicks =
        track_slider_click_count(resp.double_clicked(), copy_binding, minimum_binding, ui.input(|i| i.modifiers.shift), ui.input(|i| i.modifiers.command));
    // a click beside the thumb jumps the view there, as on the zoom bar
    if clicks == 0
        && resp.clicked()
        && let Some(q) = resp.interact_pointer_pos()
        && !thumb.contains(q)
    {
        tl.track_anim = None;
        let f = ((q.y - bar.min.y - h / 2.0) / travel).clamp(0.0, 1.0);
        *value = if invert { (1.0 - f) * max } else { f * max };
    }
    // right-click: the settings popup, always to the LEFT of the sliders (their alternate
    // position comes later), at the pointer's height
    if settings_binding || ui.rect_contains_pointer(bar) && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary)) {
        let y = ui.input(|i| i.pointer.interact_pos()).map(|p| p.y).unwrap_or(bar.center().y);
        // to the LEFT of the sliders (their alternate position is the left edge of the Timeline,
        // where that would run off the screen -- then it opens to their right instead)
        let x = if bar.min.x - 210.0 < ui.ctx().content_rect().left() { bar.max.x + 4.0 } else { bar.min.x - 210.0 };
        tl.track_slider_settings_pos = Some(pos2(x, y));
        zoombar_state_reset(ui.ctx());
        // the two settings menus are mutually exclusive: this one takes the zoom bar's with it
        tl.zoombar_settings_pos = None;
        close_zoombar_popup(ui.ctx());
    }
    (point, thumb, clicks)
}

/// The track sliders' settings popup content. Opened by right-clicking a slider and by the
/// Timeline menu's "Track Slider Settings" row. The popup only becomes scrollable when the window
/// is too small to show it whole.
pub(super) fn track_slider_settings(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    // the same emergency-only scrolling as the zoom bar's menu
    let max_h = settings_popup_max_height(ui.ctx().content_rect().height());
    let r = ui.available_rect_before_wrap();
    let r = Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + max_h));
    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
        egui::ScrollArea::vertical().id_salt("timeline-track-slider-settings-scroll").max_height(max_h).show(ui, |ui| {
            track_slider_settings_rows(app, ui);
        });
    });
}

/// The rows of the track sliders' settings popup (see [`track_slider_settings`]).
fn track_slider_settings_rows(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    /// 20% narrower than the zoom bar's settings popup (274 pt of frame).
    const CONTENT_W: f32 = 193.38;
    ui.set_max_width(CONTENT_W + 16.0);
    ui.set_min_width(CONTENT_W + 16.0);
    let t = app.tokens;
    family_reset(ui);
    // every row spans the popup's full width and centers its content in it: rows of a fixed
    // narrower width left an uneven gap on the right of the Hide Slider row
    let row_w = ui.available_width();
    let hidden = app.session.state.timeline_ui.track_slider_hidden;
    capture_controls(app, ui);
    // the top row: Hide Slider (red) while the sliders show, Unhide Slider (blue) while hidden
    let (r, _) = ui.allocate_exact_size(egui::vec2(row_w, 30.0), Sense::click());
    let hide_resp = ui.interact(r, egui::Id::unique(if hidden { "timeline-track-slider-unhide" } else { "timeline-track-slider-hide" }), Sense::click());
    if hide_resp.hovered() {
        ui.painter().rect_filled(r, 4.0, app.tokens.switch_border.gamma_multiply(0.35));
    }
    let (label, color) = if hidden { (tl!("Unhide Slider"), app.tokens.accent) } else { (tl!("Hide Slider"), Color32::from_rgb(0xf8, 0x7e, 0x7a)) };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, Tokens::ui(11.5), color);
    app.auto.add(if hidden { "timeline.trackSlider.settings.unhideSlider" } else { "timeline.trackSlider.settings.hideSlider" }, r, label);
    if hide_resp.clicked() {
        set_track_slider_hidden(app, ui.ctx(), !hidden);
        if !hidden {
            // just hidden: the "how to bring it back" window follows
            app.tl.track_hide_dialog_open = true;
        }
        app.tl.track_slider_settings_pos = None;
        ui.ctx().request_repaint();
    }
    // "Controls": not a switch -- the slider's gestures in a flyout beside the menu (the Minimap
    // Settings flyout's exact recipe, in this menu too).
    let (arow, _) = ui.allocate_exact_size(egui::vec2(row_w, 30.0), Sense::hover());
    let alabel = tl!("Controls");
    let agalley = ui.painter().layout_no_wrap(alabel.to_string(), Tokens::ui(11.5), t.text);
    ui.painter().galley(pos2(arow.center().x - agalley.size().x / 2.0, arow.center().y - agalley.size().y / 2.0), agalley, t.text);
    controls_arrow(ui, arow, CONTENT_W, r.max.x, flyout_to_the_right(ui, arow, r.max.x, CONTROLS_FLYOUT_W), t.text_dim);
    app.auto.add("timeline.trackSlider.settings.controls", arow, "Controls");
    hover_flyout(
        app,
        ui,
        egui::Id::unique("timeline-track-slider-advanced-flyout"),
        arow,
        r.max.x,
        CONTROLS_FLYOUT_W,
        true,
        "timeline.trackSlider.settings.controlsFlyout",
        "Controls",
        &[
            (tl!("Copy Other Slider:"), tl!("Double Click").to_string(), 4),
            (tl!("Minimum Track Height:"), tl!("Shift + Double Click").to_string(), 5),
            (tl!("Alternate Drag Speed:"), tl!("Shift + Drag").to_string(), 6),
            (tl!("Settings:"), tl!("Right Click").to_string(), 7),
        ],
    );
    // Smooth Animations eases the double-click moves (which land on the OTHER slider);
    // Alternate Slider Position moves the sliders to the left edge of the Timeline.
    let smooth = app.session.state.timeline_ui.track_slider_smooth;
    let c = settings_pill_row(ui, &t, row_w, tl!("Smooth Animations"), egui::Id::unique("timeline-track-slider-smooth"), smooth);
    app.auto.add("timeline.trackSlider.settings.smoothAnimations", c.rect, "Smooth Animations");
    if c.clicked() {
        app.session.state.timeline_ui.track_slider_smooth = !smooth;
        app.session.project_view_changed();
    }
    // Inverted Controls: the very same switch as the Timeline slider's menu
    let inv = app.session.state.timeline_ui.inverted_controls;
    let c = settings_pill_row(ui, &t, row_w, tl!("Invert Scrolling"), egui::Id::unique("timeline-track-slider-inverted"), inv);
    app.auto.add("timeline.trackSlider.settings.invertedControls", c.rect, "Invert Scrolling");
    if c.clicked() {
        app.session.state.timeline_ui.inverted_controls = !inv;
        app.session.project_view_changed();
    }
    // Track-boundary snapping is still under development. Keep the saved preference visible, but
    // don't present a control that appears to work while the slider ignores it.
    let snap = app.session.state.timeline_ui.track_slider_snap;
    let c = ui
        .add_enabled_ui(false, |ui| settings_pill_row(ui, &t, row_w, tl!("Snap to Tracks"), egui::Id::unique("timeline-track-slider-snap"), snap))
        .inner
        .on_hover_text(tl!("Track-boundary snapping is not available yet."));
    app.auto.add("timeline.trackSlider.settings.snapToTracks", c.rect, "Snap to Tracks");
    // Decouple Edges: an end circle drag moves only that end, the other one stays put
    let dec = app.session.state.timeline_ui.track_slider_decouple;
    let c = settings_pill_row(ui, &t, row_w, tl!("Decouple Edges"), egui::Id::unique("timeline-track-slider-decouple"), dec);
    app.auto.add("timeline.trackSlider.settings.decoupleEdges", c.rect, "Decouple Edges");
    if c.clicked() {
        app.session.state.timeline_ui.track_slider_decouple = !dec;
        app.session.project_view_changed();
    }
    let alt = app.session.state.timeline_ui.track_slider_alternate_position;
    let c = settings_pill_row(ui, &t, row_w, tl!("Alternate Slider Position"), egui::Id::unique("timeline-track-slider-alt"), alt);
    app.auto.add("timeline.trackSlider.settings.alternateSliderPosition", c.rect, "Alternate Slider Position");
    if c.clicked() {
        app.session.state.timeline_ui.track_slider_alternate_position = !alt;
        app.session.project_view_changed();
    }
    // the white divider, then Alternate Drag Speed: Slow (0.5x), Default (1x) or Custom
    ui.separator();
    ui.add_space(2.0);
    let speed = app.session.state.timeline_ui.track_slider_shift_drag_speed;
    // once the Value has been typed the tick slider stays on Custom, even when the number
    // happens to match a preset (the main slider's `shift_drag_custom` the same way)
    let custom = app.session.state.timeline_ui.track_slider_shift_drag_custom;
    let mut idx = if custom {
        2
    } else if (speed - 0.5).abs() < 0.01 {
        0
    } else if (speed - 1.0).abs() < 0.01 {
        1
    } else {
        2
    };
    settings_heading(ui, &t, tl!("Alternate Drag Speed"));
    let (resp, ch) = tick_slider(ui, &mut idx, &[tl!("Slow"), tl!("Default"), tl!("Custom")], &t);
    app.auto.add("timeline.trackSlider.settings.shiftDragSpeed", resp.rect, "Alternate Drag Speed");
    if ch {
        match idx {
            0..=1 => {
                app.session.state.timeline_ui.track_slider_shift_drag_speed = if idx == 0 { 0.5 } else { 1.0 };
                app.session.state.timeline_ui.track_slider_shift_drag_custom = false;
            }
            _ => {
                // Custom again: restore the last custom value, never reset it to a preset
                if !custom {
                    app.session.state.timeline_ui.track_slider_shift_drag_speed =
                        last_custom_value(app.session.state.timeline_ui.track_slider_shift_drag_last_custom, 10.0);
                }
                app.session.state.timeline_ui.track_slider_shift_drag_custom = true;
            }
        }
        app.session.project_view_changed();
    }
    // the Custom value row: 0.1x-10x
    ui.allocate_ui_with_layout(egui::vec2(row_w, 24.0), egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.add_sized(egui::vec2(46.0, 18.0), egui::Label::new(tl!("Value:")).wrap());
        let (response, value) = hot_number_numeric(
            ui,
            tl!("Alternate drag speed for the track sliders"),
            egui::Id::unique("timeline-track-slider-speed"),
            app.session.state.timeline_ui.track_slider_shift_drag_speed,
            HotNumberOptions::new(0.1, (0.1, 10.0), 1, "x", &app.tokens),
        );
        app.auto.add("timeline.trackSlider.settings.shiftDragSpeedValue", response.rect, "Alternate drag speed for the track sliders");
        if let Some(value) = value {
            app.session.state.timeline_ui.track_slider_shift_drag_speed = value.clamp(0.1, 10.0);
            app.session.state.timeline_ui.track_slider_shift_drag_last_custom = value.clamp(0.1, 10.0);
            app.session.state.timeline_ui.track_slider_shift_drag_custom = true;
            ui.data_mut(|d| d.insert_temp(egui::Id::unique("timeline-track-slider-view-pending"), app.session.document_epoch));
        }
    });
}

pub(crate) fn flush_track_slider_view_change(app: &mut FilmcraftApp, ctx: &egui::Context) {
    flush_track_slider_view_change_with_force(app, ctx, false);
}

fn flush_track_slider_view_change_with_force(app: &mut FilmcraftApp, ctx: &egui::Context, force: bool) {
    let id = egui::Id::unique("timeline-track-slider-view-pending");
    if let Some(document_epoch) = ctx.data(|d| d.get_temp::<u64>(id)) {
        if document_epoch != app.session.document_epoch {
            ctx.data_mut(|d| d.remove_temp::<u64>(id));
        } else if force || !ctx.input(|i| i.pointer.any_down()) {
            ctx.data_mut(|d| d.remove_temp::<u64>(id));
            app.session.project_view_changed();
        }
    }
}

/// The track sliders' settings popup opened from the Timeline's own menu ("Track Slider
/// Settings", shown only while the sliders are hidden): the same settings as right-clicking a
/// slider.
pub(super) fn track_slider_settings_popup(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let Some(pos) = app.tl.track_slider_settings_pos else {
        flush_track_slider_view_change(app, ui.ctx());
        return;
    };
    let screen = ui.ctx().content_rect();
    let pos = if pos.y + 300.0 > screen.bottom() { pos2(pos.x, (pos.y - 300.0).max(screen.top())) } else { pos };
    let panel =
        egui::Area::new(egui::Id::unique("tl-track-slider-settings")).order(egui::Order::Foreground).fixed_pos(pos).fade_in(false).interactable(true).show(
            ui.ctx(),
            |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    track_slider_settings(app, ui);
                });
            },
        );
    flush_track_slider_view_change(app, ui.ctx());
    if ui.input(|i| i.pointer.any_pressed())
        && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        && !panel.response.rect.contains(p)
        // a click on one of the settings' own popups (the Advanced flyout) is not an outside click
        && !family_contains(ui, p)
    {
        app.tl.track_slider_settings_pos = None;
    }
}

/// Hide or bring back the side track sliders (Hide Slider / Unhide Track Slider).
pub fn set_track_slider_hidden(app: &mut FilmcraftApp, _ctx: &egui::Context, hidden: bool) {
    if app.session.state.timeline_ui.track_slider_hidden == hidden {
        return;
    }
    app.session.state.timeline_ui.track_slider_hidden = hidden;
    app.session.project_view_changed();
    app.tl.track_slider_settings_pos = None;
    if !hidden {
        // a stale "how to bring it back" window means nothing once the sliders are back
        app.tl.track_hide_dialog_open = false;
    }
}
