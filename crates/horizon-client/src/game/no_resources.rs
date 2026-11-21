//! A game started without its prepared files (the game build run by hand, not
//! through the launcher): a window that says where to get the launcher
//! (`horizon_client::no_resources`) and quits when it is closed. No loader, no
//! crash report: nothing went wrong in the game.
use super::*;
use horizon_client::clipboard::{Clipboard, SystemClipboard};
use horizon_client::no_resources::{Action, DOWNLOAD_URL, NoResources};
use horizon_client::resource_import::{self, Import, Status};
use winit::event::MouseButton;

struct Missing {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    gui: UiResources,
    screen: NoResources,
    /// The pointer in physical pixels.
    pointer: (f64, f64),
    api: horizon_client::graphics::GraphicsApi,
    error: Option<Box<dyn std::error::Error>>,
    /// Where the files were looked for, to look again (iOS).
    looked_in: std::path::PathBuf,
    /// The zip being downloaded or unpacked into `looked_in` (iOS).
    import: Option<Import>,
    /// What the screen last said about it, to redraw when it changes.
    shown: Option<Status>,
}

/// Shows the screen until the player closes it.
pub(super) fn show(looked_in: &std::path::Path, settings: &Settings) -> Result<()> {
    let mut app = Missing {
        window: None,
        renderer: None,
        gui: UiResources {
            atlas: HudAtlas::build(looked_in)?,
            panorama: Arc::new(horizon_client::gfx::panorama::Faces::colour([0; 4])),
        },
        screen: if super::touch::TOUCH_CONTROLS {
            // Horizon's own strings, which need no prepared files.
            horizon_client::lang::set(horizon_client::lang::Language::load(
                &[],
                &settings.language,
            ));
            NoResources::mobile(looked_in)
        } else {
            NoResources::new(looked_in)
        },
        pointer: (0.0, 0.0),
        api: settings.horizon.api,
        error: None,
        looked_in: looked_in.to_path_buf(),
        import: None,
        shown: None,
    };
    horizon_client::hud::GUI_SCALE.store(settings.gui_scale, Ordering::Relaxed);
    EventLoop::new()?.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

impl Missing {
    fn size(&self) -> [u32; 2] {
        self.renderer
            .as_ref()
            .map_or([1280, 720], |r| [r.config.width, r.config.height])
    }

    /// `f` over a Ui of the window's size, the cursor where the pointer is.
    fn with_ui<R>(
        &self,
        batch: &mut HudBatch,
        f: impl FnOnce(&mut horizon_client::ui_theme::Ui) -> R,
    ) -> R {
        let cursor = (
            self.pointer.0 as f32 / batch.scale,
            self.pointer.1 as f32 / batch.scale,
        );
        let mut ui = horizon_client::ui_theme::Ui::new(&self.gui.atlas, batch, cursor);
        let result = f(&mut ui);
        ui.finish();
        result
    }

    fn draw(&mut self) {
        super::follow_window_size(self.window.as_deref(), self.renderer.as_mut());
        let mut batch = HudBatch::new(self.size());
        self.with_ui(&mut batch, |ui| self.screen.draw(ui));
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        match renderer.render(&loading_frame(&batch)) {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => log::warn!("Horizon: frame failed: {e}"),
        }
        // The frame was not shown (the first one often isn't): the glyphs it
        // rasterized never reached the atlas, so draw them again next frame.
        horizon_client::ui_theme::reset_atlas();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn click(&mut self, event_loop: &ActiveEventLoop) {
        let mut batch = HudBatch::new(self.size());
        match self.with_ui(&mut batch, |ui| self.screen.click(ui)) {
            Some(Action::Download) => packs::open_uri(DOWNLOAD_URL),
            Some(Action::Quit) => event_loop.exit(),
            Some(Action::Check) => {
                // A zip beside `resources` is unpacked into it: no unzipping in the Files app.
                if self.import.is_none()
                    && super::missing_resources(&self.looked_in).is_some()
                    && resource_import::find_archive(&self.folder()).is_some()
                {
                    self.start_import(None);
                    return;
                }
                let found = super::missing_resources(&self.looked_in).is_none();
                log::info!("Horizon: looked for the game files again: found {found}");
                if found {
                    // iOS runs one event loop for the app's life: the game starts
                    // on the next launch.
                    self.screen.found();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Some(Action::Fetch) => {
                let copied = SystemClipboard::default().get();
                match resource_import::download_link(&copied) {
                    Some(url) => self.start_import(Some(url)),
                    None => {
                        let note = horizon_client::lang::get("horizon.mobile.no_link");
                        self.screen.busy(false, Some(note));
                        self.redraw();
                    }
                }
            }
            Some(Action::Cancel) => {
                if let Some(import) = &self.import {
                    import.cancel();
                }
            }
            None => {}
        }
    }

    /// The folder holding `resources` (the app's Documents on iOS): where a zip is put.
    fn folder(&self) -> std::path::PathBuf {
        let root = std::path::absolute(&self.looked_in).unwrap_or_else(|_| self.looked_in.clone());
        root.parent().map_or_else(|| root.clone(), std::path::Path::to_path_buf)
    }

    fn start_import(&mut self, url: Option<String>) {
        let root = std::path::absolute(&self.looked_in).unwrap_or_else(|_| self.looked_in.clone());
        log::info!("Horizon: importing the game files ({})", if url.is_some() { "link" } else { "zip" });
        self.import = Some(Import::start(url, self.folder(), root));
        self.shown = None;
        self.screen.busy(true, None);
        self.redraw();
    }

    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Show an import's progress, and its end: the files found, or why it failed.
    fn poll_import(&mut self) {
        let Some(import) = self.import.clone() else {
            return;
        };
        let status = import.status();
        if self.shown.as_ref() == Some(&status) {
            return;
        }
        self.shown = Some(status.clone());
        let get = horizon_client::lang::get;
        match status {
            Status::Idle => {}
            Status::Downloading(done, Some(total)) => self.screen.busy(
                true,
                Some(
                    get("horizon.mobile.downloading")
                        .replace("{done}", &megabytes(done))
                        .replace("{total}", &megabytes(total)),
                ),
            ),
            Status::Downloading(done, None) => self.screen.busy(
                true,
                Some(get("horizon.mobile.downloading_unknown").replace("{done}", &megabytes(done))),
            ),
            Status::Unpacking(done, total) => self.screen.busy(
                true,
                Some(
                    get("horizon.mobile.unpacking")
                        .replace("{done}", &done.to_string())
                        .replace("{total}", &total.to_string()),
                ),
            ),
            Status::Done => {
                self.import = None;
                log::info!("Horizon: the game files were imported");
                self.screen.found();
            }
            Status::Failed(error) => {
                self.import = None;
                log::warn!("Horizon: importing the game files failed: {error}");
                self.screen.busy(
                    false,
                    Some(get("horizon.mobile.failed").replace("{error}", &error)),
                );
            }
        }
        self.redraw();
    }
}

/// `bytes` in megabytes, for the progress line.
fn megabytes(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / f64::from(1u32 << 30))
    } else {
        format!("{:.1} MB", bytes as f64 / f64::from(1u32 << 20))
    }
}

impl ApplicationHandler for Missing {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Horizon")
            .with_window_icon(horizon_client::window::icon());
        let attributes = super::window_size(attributes, [1040.0, 700.0]);
        let made = event_loop
            .create_window(attributes)
            .map_err(Into::into)
            .and_then(|window| {
                let window = Arc::new(window);
                let renderer = loading_renderer(
                    window.clone(),
                    false,
                    Some(&self.gui.atlas),
                    Some(&self.gui.panorama),
                    None,
                    self.api,
                )?;
                Ok((window, renderer))
            });
        match made {
            Ok((window, renderer)) => {
                window.request_redraw();
                self.window = Some(window);
                self.renderer = Some(renderer);
            }
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone().filter(|w| w.id() == id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            // iOS sends touches and no mouse: the first finger is the mouse.
            WindowEvent::Touch(touch) if super::touch::TOUCH_CONTROLS => {
                thread_local! {
                    static TOUCH: std::cell::RefCell<horizon_client_input::touch::TouchMouse> =
                        Default::default();
                }
                for event in TOUCH.with_borrow_mut(|t| t.translate(WindowEvent::Touch(touch))) {
                    self.window_event(event_loop, id, event);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.click(event_loop),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.physical_key == PhysicalKey::Code(KeyCode::Escape) =>
            {
                event_loop.exit()
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if horizon_threads::stop_requests() > 0 {
            if let Some(import) = &self.import {
                import.cancel();
            }
            event_loop.exit();
            return;
        }
        self.poll_import();
        // Wake now and then for Ctrl+C in the console; redraws come with events.
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(250),
        ));
        // iOS may show the window after its first frames were presented, and
        // sends no event that would draw it again: draw at each wake there.
        if super::touch::TOUCH_CONTROLS
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}
