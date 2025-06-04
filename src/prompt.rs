use std::{io, sync::Arc, time::Duration};

use arboard::Clipboard;
use futures::StreamExt;
use promkit_core::{
    crossterm::{
        cursor,
        event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers},
        execute,
        style::{Color, ContentStyle},
        terminal::{self, disable_raw_mode, enable_raw_mode},
    },
    PaneFactory,
};
use promkit_widgets::text::{self, Text};
use tokio::{
    sync::{mpsc, Mutex, RwLock},
    task::JoinHandle,
};

use crate::{
    config::{event::Matcher, Keybinds, ReactivityControl},
    Context, ContextMonitor, Editor, PaneIndex, Processor, Renderer, SearchProvider,
    SpinnerSpawner, ViewInitializer, ViewProvider, Visualizer, EMPTY_PANE,
};

fn spawn_debouncer<T: Send + 'static>(
    mut debounce_rx: mpsc::Receiver<T>,
    last_tx: mpsc::Sender<T>,
    duration: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_query = None;
        let mut delay = tokio::time::interval(duration);
        loop {
            tokio::select! {
                maybe_query = debounce_rx.recv() => {
                    if let Some(query) = maybe_query {
                        last_query = Some(query);
                    } else {
                        break;
                    }
                },
                _ = delay.tick() => {
                    if let Some(text) = last_query.take() {
                        let _ = last_tx.send(text).await;
                    }
                },
            }
        }
    })
}

fn copy_to_clipboard(content: &str) -> text::State {
    match Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(content) {
            Ok(_) => text::State {
                text: Text::from("Copied to clipboard"),
                style: ContentStyle {
                    foreground_color: Some(Color::Green),
                    ..Default::default()
                },
                ..Default::default()
            },
            Err(e) => text::State {
                text: Text::from(format!("Failed to copy to clipboard: {}", e)),
                style: ContentStyle {
                    foreground_color: Some(Color::Red),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        // arboard fails (in the specific environment like linux?) on Clipboard::new()
        // suppress the errors (but still show them) not to break the prompt
        // https://github.com/1Password/arboard/issues/153
        Err(e) => text::State {
            text: Text::from(format!("Failed to setup clipboard: {}", e)),
            style: ContentStyle {
                foreground_color: Some(Color::Red),
                ..Default::default()
            },
            ..Default::default()
        },
    }
}

enum Focus {
    Editor,
    Processor,
}

#[allow(clippy::too_many_arguments)]
pub async fn run<T: ViewProvider + SearchProvider>(
    item: &'static str,
    reactivity_control: ReactivityControl,
    provider: &mut T,
    editor: Editor,
    loading_suggestions_task: JoinHandle<anyhow::Result<()>>,
    no_hint: bool,
    keybinds: Keybinds,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), cursor::Hide)?;

    let size = terminal::size()?;

    let shared_renderer = Arc::new(Mutex::new(Renderer::try_init_draw(
        [
            editor.create_editor_pane(size.0, size.1),
            EMPTY_PANE.to_owned(),
            EMPTY_PANE.to_owned(),
            EMPTY_PANE.to_owned(),
        ],
        no_hint,
    )?));

    let ctx = Arc::new(Mutex::new(Context::new(size)));

    let (last_query_tx, mut last_query_rx) = mpsc::channel(1);
    let (debounce_query_tx, debounce_query_rx) = mpsc::channel(1);
    let query_debouncer = spawn_debouncer(
        debounce_query_rx,
        last_query_tx,
        reactivity_control.query_debounce_duration,
    );
    if !editor.text().is_empty() {
        debounce_query_tx.send(editor.text()).await?;
    }

    let (last_resize_tx, mut last_resize_rx) = mpsc::channel::<(u16, u16)>(1);
    let (debounce_resize_tx, debounce_resize_rx) = mpsc::channel(1);
    let resize_debouncer = spawn_debouncer(
        debounce_resize_rx,
        last_resize_tx,
        reactivity_control.resize_debounce_duration,
    );

    let spinner_spawner = SpinnerSpawner::new(ctx.clone());
    let spinning =
        spinner_spawner.spawn_spin_task(shared_renderer.clone(), reactivity_control.spin_duration);

    let mut focus = Focus::Editor;
    let (editor_event_tx, mut editor_event_rx) = mpsc::channel::<Event>(1);
    let (processor_event_tx, mut processor_event_rx) = mpsc::channel::<Event>(1);

    let (editor_copy_tx, mut editor_copy_rx) = mpsc::channel::<()>(1);
    let (processor_copy_tx, mut processor_copy_rx) = mpsc::channel::<()>(1);

    let (editor_focus_tx, mut editor_focus_rx) = mpsc::channel::<bool>(1);

    let mut text_diff = [editor.text(), editor.text()];
    let shared_editor = Arc::new(RwLock::new(editor));
    let processor = Processor::new(ctx.clone());
    let context_monitor = ContextMonitor::new(ctx.clone());
    let initializer = ViewInitializer::new(ctx.clone());
    let initializing = initializer.initialize(
        provider,
        item,
        size,
        shared_renderer.clone(),
        keybinds.on_json_viewer,
    );

    let main_task: JoinHandle<anyhow::Result<()>> = {
        let mut stream = EventStream::new();
        let shared_renderer = shared_renderer.clone();
        tokio::spawn(async move {
            'main: loop {
                tokio::select! {
                    Some(Ok(event)) = stream.next() => {
                        match event {
                            Event::Resize(width, height) => {
                                debounce_resize_tx.send((width, height)).await?;
                            },
                            event if keybinds.exit.matches(&event) => {
                                break 'main
                            },
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('q'),
                                modifiers: KeyModifiers::CONTROL,
                                kind: KeyEventKind::Press,
                                state: KeyEventState::NONE,
                            }) => {
                                editor_copy_tx.send(()).await?;
                            },
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('o'),
                                modifiers: KeyModifiers::CONTROL,
                                kind: KeyEventKind::Press,
                                state: KeyEventState::NONE,
                            }) => {
                                let mut pane = EMPTY_PANE.to_owned();
                                if context_monitor.is_idle().await {
                                    processor_copy_tx.send(()).await?;
                                } else {
                                    let size = terminal::size()?;
                                    pane = text::State {
                                        text: Text::from("Failed to copy while rendering is in progress.".to_string()),
                                        style: ContentStyle {
                                            foreground_color: Some(Color::Yellow),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    }.create_pane(size.0, size.1);
                                }
                                {
                                    shared_renderer.lock().await.update_and_draw([
                                        (PaneIndex::Guide, pane),
                                    ])?;
                                }
                            },
                            Event::Key(KeyEvent {
                                code: KeyCode::Down,
                                modifiers: KeyModifiers::SHIFT,
                                kind: KeyEventKind::Press,
                                state: KeyEventState::NONE,
                            }) | Event::Key(KeyEvent {
                                code: KeyCode::Up,
                                modifiers: KeyModifiers::SHIFT,
                                kind: KeyEventKind::Press,
                                state: KeyEventState::NONE,
                            }) => {
                                match focus {
                                    Focus::Editor => {
                                        let mut pane = EMPTY_PANE.to_owned();
                                        if context_monitor.is_idle().await {
                                            focus = Focus::Processor;
                                            editor_focus_tx.send(false).await?;
                                        } else {
                                            let size = terminal::size()?;
                                            pane = text::State {
                                                text: Text::from("Failed to switch pane while rendering is in progress.".to_string()),
                                                style: ContentStyle {
                                                    foreground_color: Some(Color::Yellow),
                                                    ..Default::default()
                                                },
                                                ..Default::default()
                                            }.create_pane(size.0, size.1);
                                        }
                                        {
                                            shared_renderer.lock().await.update_and_draw([
                                                (PaneIndex::Guide, pane),
                                            ])?;
                                        }
                                    },
                                    Focus::Processor => {
                                        focus = Focus::Editor;
                                        editor_focus_tx.send(true).await?;
                                    },
                                }
                            },
                            event => {
                                match focus {
                                    Focus::Editor => {
                                        editor_event_tx.send(event).await?;
                                    },
                                    Focus::Processor => {
                                        processor_event_tx.send(event).await?;
                                    },
                                }
                            },
                        }
                    },
                    else => {
                        break 'main;
                    }
                }
            }
            Ok(())
        })
    };

    let editor_task: JoinHandle<anyhow::Result<()>> = {
        let shared_renderer = shared_renderer.clone();
        let shared_editor = shared_editor.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(focus) = editor_focus_rx.recv() => {
                        let term_size = terminal::size()?;
                        let (editor_pane, guide_pane) = {
                            let mut editor = shared_editor.write().await;
                            if focus {
                                editor.focus();
                            } else {
                                editor.defocus();
                            }
                            (
                                editor.create_editor_pane(term_size.0, term_size.1),
                                editor.create_guide_pane(term_size.0, term_size.1),
                            )
                        };
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Editor, editor_pane),
                                (PaneIndex::Guide, guide_pane),
                            ])?;
                        }
                    }
                    Some(()) = editor_copy_rx.recv() => {
                        let text = {
                            let editor = shared_editor.write().await;
                            editor.text()
                        };
                        let guide = copy_to_clipboard(&text);
                        let size = terminal::size()?;
                        let pane = guide.create_pane(size.0, size.1);
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Guide, pane),
                            ])?;
                        }
                    }
                    Some(event) = editor_event_rx.recv() => {
                        let size = terminal::size()?;
                        let (editor_pane, guide_pane, searcher_pane) = {

                            let mut editor = shared_editor.write().await;
                            editor.operate(&event).await?;

                            let current_text = editor.text();
                            if current_text != text_diff[1] {
                                debounce_query_tx.send(current_text.clone()).await?;
                                text_diff[0] = text_diff[1].clone();
                                text_diff[1] = current_text;
                            }
                            (
                                editor.create_editor_pane(size.0, size.1),
                                editor.create_guide_pane(size.0, size.1),
                                editor.create_searcher_pane(size.0, size.1),
                            )
                        };
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Editor, editor_pane),
                                (PaneIndex::Guide, guide_pane),
                                (PaneIndex::Search, searcher_pane),
                            ])?;
                        }
                    }
                    else => {
                        break
                    }
                }
            }
            Ok(())
        })
    };

    let processor_task: JoinHandle<anyhow::Result<()>> = {
        let shared_renderer = shared_renderer.clone();
        let shared_editor = shared_editor.clone();
        let visualizer = initializing.await?;
        let shared_visualizer = Arc::new(Mutex::new(visualizer));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(()) = processor_copy_rx.recv() => {
                        let visualizer = shared_visualizer.lock().await;
                        let guide = copy_to_clipboard(&visualizer.content_to_copy().await);
                        let size = terminal::size()?;
                        let pane = guide.create_pane(size.0, size.1);
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Guide, pane),
                            ])?;
                        }
                    }
                    Some(event) = processor_event_rx.recv() => {
                        let term_size = terminal::size()?;
                        let pane = {
                            let mut visualizer = shared_visualizer.lock().await;
                            visualizer.create_pane_from_event((term_size.0, term_size.1), &event).await
                        };
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Processor, pane),
                            ])?;
                        }
                    }
                    Some(query) = last_query_rx.recv() => {
                        processor.render_result(
                            shared_visualizer.clone(),
                            query,
                            shared_renderer.clone(),
                        ).await;
                    }
                    Some(area) = last_resize_rx.recv() => {
                        let (editor_pane, guide_pane, searcher_pane) = {
                            let editor = shared_editor.read().await;
                            (
                                editor.create_editor_pane(area.0, area.1),
                                editor.create_guide_pane(area.0, area.1),
                                editor.create_searcher_pane(area.0, area.1),
                            )
                        };
                        {
                            shared_renderer.lock().await.update_and_draw([
                                (PaneIndex::Editor, editor_pane),
                                (PaneIndex::Guide, guide_pane),
                                (PaneIndex::Search, searcher_pane),
                            ])?;
                        }
                        let text = {
                            let editor = shared_editor.read().await;
                            editor.text()
                        };
                        processor.render_on_resize(
                            shared_visualizer.clone(),
                            area,
                            text,
                            shared_renderer.clone(),
                        ).await;
                    }
                    else => {
                        break
                    }
                }
            }
            Ok(())
        })
    };

    main_task.await??;

    loading_suggestions_task.abort();
    spinning.abort();
    query_debouncer.abort();
    resize_debouncer.abort();
    editor_task.abort();
    processor_task.abort();

    execute!(io::stdout(), cursor::Show)?;
    disable_raw_mode()?;

    Ok(())
}
