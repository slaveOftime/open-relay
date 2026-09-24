use std::{
    io::{self, IsTerminal},
    panic::AssertUnwindSafe,
    time::Instant,
};

use crossterm::event::{Event, KeyEventKind};
use futures_util::FutureExt;

use super::super::list::{ListTarget, fetch_node_names, list_targets_for_all_nodes};
use super::app::App;
use super::app::open_selected_inline;
use super::constants::{
    ANIMATION_REDRAW_INTERVAL, INPUT_POLL_INTERVAL, REDRAW_INTERVAL, REFRESH_INTERVAL,
};
use super::keys::{AppAction, route_key};
use super::refresh::{
    SessionRefresh, apply_refresh, drain_pending_events, fetch_sessions, panic_payload_message,
    read_terminal_event, remove_sessions, start_clone, stop_session, update_session,
};
use super::terminal::TuiTerminal;
use super::view::render;
use crate::{
    cli::ListArgs,
    config::AppConfig,
    error::{AppError, Result},
};

pub async fn run(config: &AppConfig, args: &ListArgs, targets: Vec<ListTarget>) -> Result<()> {
    match AssertUnwindSafe(run_inner(config, args, targets))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(payload) => Err(AppError::Protocol(format!(
            "interactive session list crashed: {}",
            panic_payload_message(payload.as_ref())
        ))),
    }
}

async fn fetch_follow_refresh(
    config: &AppConfig,
    query: crate::protocol::ListQuery,
    targets: &[ListTarget],
    node_all: bool,
    known_nodes: Vec<String>,
) -> Result<SessionRefresh> {
    let targets = if node_all {
        let mut nodes = fetch_node_names(config).await?;
        nodes.extend(known_nodes);
        nodes.sort();
        nodes.dedup();
        list_targets_for_all_nodes(nodes)
    } else {
        targets.to_vec()
    };
    fetch_sessions(config, query, &targets).await
}

async fn run_inner(config: &AppConfig, args: &ListArgs, targets: Vec<ListTarget>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::Protocol(
            "--follow requires an interactive terminal".to_string(),
        ));
    }
    #[cfg(windows)]
    crate::client::crash::install();

    let query = super::super::list::build_list_query(args)?;
    let mut app = App {
        show_node: args.node_all || targets.len() > 1,
        session_storage_dir: Some(config.paths.sessions_dir.clone()),
        ..Default::default()
    };
    // When the list shows exactly one node's sessions, dialogs that start or
    // update sessions default to that node — Ctrl+N in a `--node worker`
    // view must create the session on `worker`, not locally (where it would
    // never appear in the filtered list).
    let list_node = match targets.as_slice() {
        [target] => target.node.as_deref(),
        _ => None,
    };
    let refresh =
        fetch_follow_refresh(config, query.clone(), &targets, args.node_all, Vec::new()).await;
    crate::metrics::mark("tui: sessions fetched");
    match refresh {
        Ok(refresh) => {
            app.set_refresh_message(refresh.warning());
            app.replace_sessions(refresh.sessions);
        }
        Err(error) => app.set_refresh_message(Some(format!("sync lost: {error}"))),
    }
    let mut terminal = TuiTerminal::new()?;
    crate::metrics::mark("tui: terminal ready");
    let mut last_refresh = Instant::now();
    let mut last_draw = Instant::now();
    let mut redraw = true;
    // Refreshes run on a spawned task so a slow daemon/node response never
    // stalls input handling; results come back through this channel.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<Result<SessionRefresh>>(1);
    let mut refresh_in_flight = false;

    'input: loop {
        // Wait briefly for the next event, then drain everything already
        // queued before drawing: a fast typing burst (or paste) is applied
        // as one batch and rendered with a single frame instead of one
        // frame per keystroke.
        let mut events = Vec::new();
        if let Some(event) = read_terminal_event(INPUT_POLL_INTERVAL)? {
            events.push(event);
            drain_pending_events(&mut events)?;
        }
        for event in events {
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    match route_key(&mut app, key, list_node) {
                        AppAction::None => {}
                        AppAction::Quit => break 'input,
                        AppAction::OpenInline => {
                            open_selected_inline(&mut terminal, &mut app, None)?
                        }
                        AppAction::Start(launch) => {
                            start_clone(config, &mut terminal, &mut app, launch).await?
                        }
                        AppAction::Update(update) => update_session(config, &mut app, update).await,
                        AppAction::Stop(target) => stop_session(config, &mut app, target),
                        AppAction::Remove(targets) => remove_sessions(config, &mut app, targets),
                    }
                    redraw = true;
                }
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL && !refresh_in_flight {
            refresh_in_flight = true;
            last_refresh = Instant::now();
            let tx = refresh_tx.clone();
            let config = config.clone();
            let query = query.clone();
            let targets = targets.clone();
            let node_all = args.node_all;
            let known_nodes = app
                .sessions
                .iter()
                .filter_map(|session| session.node.clone())
                .collect();
            tokio::spawn(async move {
                let _ = tx
                    .send(
                        fetch_follow_refresh(&config, query, &targets, node_all, known_nodes).await,
                    )
                    .await;
            });
        }
        while let Ok(result) = refresh_rx.try_recv() {
            refresh_in_flight = false;
            apply_refresh(&mut app, &query, result);
            redraw = true;
        }

        // Effects (attention pulse, fade-ins) need a faster frame cadence;
        // the idle list ticks over at the slower refresh rate.
        let redraw_interval = if app.effects.is_running() {
            ANIMATION_REDRAW_INTERVAL
        } else {
            REDRAW_INTERVAL
        };
        if redraw || last_draw.elapsed() >= redraw_interval {
            terminal.draw(|frame| render(frame, &mut app))?;
            last_draw = Instant::now();
            redraw = false;
        }
    }

    terminal.teardown()?;
    Ok(())
}
