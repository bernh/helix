use std::collections::HashSet;
use std::time::Duration;

use futures_util::stream::FuturesOrdered;
use helix_core::syntax::config::LanguageServerFeature;
use helix_event::{register_hook, send_blocking, TaskController, TaskHandle};
use helix_view::document::Mode;
use helix_view::events::SelectionDidChange;
use helix_view::handlers::lsp::HoverEvent;
use helix_view::Editor;
use tokio::time::Instant;
use tokio_stream::StreamExt;

use crate::events::OnModeSwitch;
use crate::handlers::Handlers;
use crate::job;

/// Debounce timeout in milliseconds for auto-hover
const TIMEOUT: u64 = 200;

#[derive(Debug)]
pub(super) struct HoverHandler {
    task_controller: TaskController,
}

impl HoverHandler {
    pub fn new() -> HoverHandler {
        HoverHandler {
            task_controller: TaskController::new(),
        }
    }
}

impl helix_event::AsyncHook for HoverHandler {
    type Event = HoverEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _timeout: Option<tokio::time::Instant>,
    ) -> Option<Instant> {
        match event {
            HoverEvent::Trigger => Some(Instant::now() + Duration::from_millis(TIMEOUT)),
            HoverEvent::Cancel => {
                self.task_controller.cancel();
                None
            }
        }
    }

    fn finish_debounce(&mut self) {
        let handle = self.task_controller.restart();
        job::dispatch_blocking(move |editor, _compositor| request_hover(editor, handle))
    }
}

fn request_hover(editor: &mut Editor, cancel: TaskHandle) {
    // Only send hover requests in Normal mode
    if editor.mode != Mode::Normal {
        return;
    }

    let (view, doc) = current!(editor);

    let mut seen_language_servers = HashSet::new();
    let futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::Hover)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .map(|language_server| {
            let pos = doc.position(view.id, language_server.offset_encoding());
            language_server
                .text_document_hover(doc.identifier(), pos, None)
                .unwrap()
        })
        .collect();

    if futures.is_empty() {
        return;
    }

    // Spawn task to await hover responses (for LSP side effects)
    // No UI is displayed - this is silent/fire-and-forget
    tokio::spawn(async move {
        let mut futures = futures;

        loop {
            tokio::select! {
                biased;
                _ = cancel.canceled() => {
                    return;
                }
                response = futures.next() => {
                    match response {
                        Some(Err(err)) => log::error!("Error requesting hover: {err}"),
                        Some(Ok(_)) => (),
                        None => break,
                    }
                }
            }
        }
    });
}

pub fn register_hooks(handlers: &Handlers) {
    let tx = handlers.hover.clone();
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        // Only trigger if auto_hover is enabled
        // Mode check is done when processing the request
        if event.doc.config.load().lsp.auto_hover {
            send_blocking(&tx, HoverEvent::Trigger);
        }
        Ok(())
    });

    let tx = handlers.hover.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        // Cancel hover when leaving Normal mode
        if event.old_mode == Mode::Normal && event.new_mode != Mode::Normal {
            send_blocking(&tx, HoverEvent::Cancel);
        }
        Ok(())
    });
}
