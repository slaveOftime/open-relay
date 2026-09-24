use std::time::Instant;

use ratatui::{Frame, layout::Rect, style::Color};
use tachyonfx::{
    CellFilter, CellIterator, ColorSpace, Duration as FxDuration, EffectTimer, Interpolation,
    RefRect, fx,
};

use super::app::App;
use super::constants::{ATTENTION_PULSE_BG, ATTENTION_PULSE_BG_SELECTED};
use super::dialog_render::centered_rect;

pub fn attention_pulse_key(session_key: &str) -> String {
    format!("attention-pulse:{session_key}")
}

pub fn render_effects(
    frame: &mut Frame<'_>,
    app: &mut App,
    message_area: Option<Rect>,
    attention_rows: Vec<(String, Rect, bool)>,
) {
    // Pulse the background of every row whose session waits for input. The
    // filter is the row's RefRect alone (updated every frame, so the pulse
    // follows the row across scrolling, reordering and resizes), and only the
    // background is animated — every foreground colour (status semantics,
    // dimming, selection) keeps working on top of the dark amber tint.
    // Nothing outside those rows is ever touched.
    let stale: Vec<String> = app
        .attention_rows
        .keys()
        .filter(|key| !attention_rows.iter().any(|(active, _, _)| active == *key))
        .cloned()
        .collect();
    for key in stale {
        app.effects.cancel_unique_effect(attention_pulse_key(&key));
        app.attention_rows.remove(&key);
    }
    for (key, rect, selected) in attention_rows {
        let existing = app
            .attention_rows
            .get(&key)
            .map(|(row, was_selected)| (row.clone(), *was_selected));
        if let Some((row, was_selected)) = existing {
            row.set(rect);
            if was_selected == selected {
                continue;
            }
            // The selection state changed the pulse target: swap the effect.
            app.effects.cancel_unique_effect(attention_pulse_key(&key));
            app.attention_rows.remove(&key);
        }
        // The filter must be attached to the inner effect: the repeating /
        // ping-pong containers do not apply their own filter to the wrapped
        // effect's cells.
        //
        // tachyonfx has no `fade_to_bg`, so the pulse is a small custom
        // shader that lerps only the background colour of the row's cells
        // (selected by the RefRect filter) towards the attention tint.
        let row = RefRect::new(rect);
        let target = if selected {
            ATTENTION_PULSE_BG_SELECTED
        } else {
            ATTENTION_PULSE_BG
        };
        let pulse = fx::effect_fn(
            (),
            EffectTimer::from_ms(800, Interpolation::SineInOut),
            move |_, context: fx::ShaderFnContext<'_>, cells: CellIterator<'_>| {
                let alpha = context.alpha();
                cells.for_each_cell(|_, cell| {
                    let bg = ColorSpace::Rgb.lerp(&cell.bg, &target, alpha);
                    cell.set_bg(bg);
                });
            },
        )
        .with_filter(CellFilter::RefArea(row.clone()));
        app.effects.add_unique_effect(
            attention_pulse_key(&key),
            fx::repeating(fx::ping_pong(pulse)),
        );
        app.attention_rows.insert(key, (row, selected));
    }

    // Fade in a freshly posted status message.
    if app.rendered_message != app.message {
        app.rendered_message = app.message.clone();
        if let (Some(_), Some(area)) = (app.message.as_ref(), message_area) {
            app.effects.add_unique_effect(
                "message-fade",
                fx::fade_from_fg(
                    Color::DarkGray,
                    EffectTimer::from_ms(400, Interpolation::QuadOut),
                )
                .with_area(area),
            );
        }
    }

    // Fade a clone/update dialog in when it opens.
    let dialog = if let Some(clone) = app.clone_dialog.as_ref() {
        let height = if clone.source_id.is_some() { 20 } else { 19 };
        Some(("clone-fade", centered_rect(frame.area(), 96, height)))
    } else if app.update_dialog.is_some() {
        Some(("update-fade", centered_rect(frame.area(), 110, 19)))
    } else {
        None
    };
    match dialog {
        Some((key, area)) if app.rendered_dialog != Some(key) => {
            app.rendered_dialog = Some(key);
            app.effects.add_unique_effect(
                key,
                fx::fade_from(
                    Color::Reset,
                    Color::Reset,
                    EffectTimer::from_ms(240, Interpolation::QuadOut),
                )
                .with_area(area),
            );
        }
        None => app.rendered_dialog = None,
        _ => {}
    }

    let elapsed = app
        .last_frame_at
        .map(|instant| instant.elapsed())
        .unwrap_or_default();
    app.last_frame_at = Some(Instant::now());
    let fx_elapsed = FxDuration::from_millis(elapsed.as_millis().min(u32::MAX as u128) as u32);
    let area = frame.area();
    app.effects
        .process_effects(fx_elapsed, frame.buffer_mut(), area);
}
