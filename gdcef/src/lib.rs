mod webrender;
mod utils;

use cef::{BrowserSettings, ImplBrowser, ImplBrowserHost, MouseButtonType, MouseEvent, RequestContextSettings, Settings, WindowInfo, api_hash, quit_message_loop, run_message_loop};
use cef::sys::cef_event_flags_t;
use cef_app::FrameBuffer;
use godot::classes::notify::ControlNotification;
use godot::classes::{ITextureRect, Image, ImageTexture, InputEvent, InputEventMouseButton, InputEventMouseMotion, Os, TextureRect};
use godot::classes::texture_rect::ExpandMode;
use godot::classes::image::Format as ImageFormat;
use godot::global::{MouseButton, MouseButtonMask};
use godot::init::*;
use godot::prelude::*;
use winit::dpi::{PhysicalSize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

use crate::utils::get_subprocess_path;

struct GodotCef;
#[gdextension]
unsafe impl ExtensionLibrary for GodotCef {}

struct App {
    browser: Option<cef::Browser>,
    frame_buffer: Option<Arc<Mutex<FrameBuffer>>>,
    texture: Option<Gd<ImageTexture>>,
    render_size: Option<Arc<Mutex<PhysicalSize<f32>>>>,
    device_scale_factor: Option<Arc<Mutex<f32>>>,
    last_size: Vector2,
    last_dpi: f32,
}

impl Default for App {
    fn default() -> Self {
        Self {
            browser: None,
            frame_buffer: None,
            texture: None,
            render_size: None,
            device_scale_factor: None,
            last_size: Vector2::ZERO,
            last_dpi: 1.0,
        }
    }
}

#[derive(GodotClass)]
#[class(base=TextureRect)]
struct CefTexture {
    base: Base<TextureRect>,

    // internal states
    app: App,

    #[export]
    url: GString,
}

#[godot_api]
impl ITextureRect for CefTexture {
    fn init(base: Base<TextureRect>) -> Self {
        Self {
            base,
            app: App::default(),
            url: "https://google.com".into(),
        }
    }

    fn ready(&mut self) {
        self.on_ready();
    }

    fn process(&mut self, _delta: f64) {
        self.on_process();
    }

    fn on_notification(&mut self, what: ControlNotification) {
        match what {
            ControlNotification::WM_CLOSE_REQUEST => {
                self.shutdown_cef();
            }
            _ => {}
        }
    }

    // fn gui_input(&mut self, event: Gd<InputEvent>) {
    //     self.handle_input_event(event);
    // }

    fn input(&mut self, event: Gd<InputEvent>) {
        self.handle_input_event(event);
    }
}

#[godot_api]
impl CefTexture {
    fn load_cef_framework() {
        #[cfg(target_os = "macos")]
        {
            use cef::sys::cef_load_library;

            let framework_path = utils::get_framework_path();
            let path = framework_path
                .unwrap()
                .join("Chromium Embedded Framework")
                .canonicalize()
                .unwrap();

            use std::os::unix::ffi::OsStrExt;
            let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
                panic!("Failed to convert library path to CString");
            };
            let result = unsafe {
                let arg_path = Some(&*path.as_ptr().cast());
                let arg_path = arg_path.map(std::ptr::from_ref).unwrap_or(std::ptr::null());
                cef_load_library(arg_path) == 1
            };

            assert!(result, "Failed to load macOS CEF framework");

            // set the API hash
            let _ = api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        };
    }

    #[cfg(all(target_os = "macos"))]
    fn load_sandbox(args: &cef::MainArgs) {
        use libloading::Library;

        let framework_path = utils::get_framework_path();
        let path = framework_path
            .unwrap()
            .join("Libraries/libcef_sandbox.dylib")
            .canonicalize()
            .unwrap();

        unsafe {
            let lib = Library::new(path).unwrap();
            let func = lib.get::<unsafe extern "C" fn(
                argc: std::os::raw::c_int,
                argv: *mut *mut ::std::os::raw::c_char,
            )>(b"cef_sandbox_initialize\0").unwrap();
            func(args.argc, args.argv);
        }
    }

    fn initialize_cef() {
        let args = cef::args::Args::new();
        let mut app = cef_app::AppBuilder::build(cef_app::OsrApp::new());

        #[cfg(all(target_os = "macos"))]
        Self::load_sandbox(args.as_main_args());

        // FIXME: cross-platform
        let subprocess_path = get_subprocess_path().unwrap();

        godot_print!("subprocess_path: {}", subprocess_path.to_str().unwrap());

        let user_data_dir = PathBuf::from(Os::singleton().get_user_data_dir().to_string());
        let root_cache_path = user_data_dir.join("Godot CEF/Cache");

        let settings = Settings {
            browser_subprocess_path: subprocess_path.to_str().unwrap().into(),
            windowless_rendering_enabled: true as _,
            external_message_pump: true as _,
            log_severity: cef::LogSeverity::VERBOSE as _,
            // log_file: "/tmp/cef.log".into(),
            root_cache_path: root_cache_path.to_str().unwrap().into(),
            ..Default::default()
        };

        #[cfg(target_os = "macos")]
        let settings = Settings {
            framework_dir_path: utils::get_framework_path().unwrap().to_str().unwrap().into(),
            main_bundle_path: get_subprocess_path().unwrap().join("../../..").canonicalize().unwrap().to_str().unwrap().into(),
            ..settings
        };

        let ret = cef::initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut()
        );

        assert_eq!(ret, 1, "failed to initialize CEF");
    }

    fn shutdown_cef(&mut self) {
        self.app.browser = None;
        self.app.frame_buffer = None;
        self.app.texture = None;
        self.app.render_size = None;
        self.app.device_scale_factor = None;

        if CEF_INITIALIZED.is_completed() {
            cef::shutdown();
        }
    }

    fn create_texture_and_buffer(&mut self, render_handler: &cef_app::OsrRenderHandler, initial_dpi: f32) {
        let frame_buffer = render_handler.get_frame_buffer();
        let render_size = render_handler.get_size();
        let device_scale_factor = render_handler.get_device_scale_factor();

        let texture = ImageTexture::new_gd();
        self.base_mut().set_texture(&texture);

        self.app.frame_buffer = Some(frame_buffer);
        self.app.texture = Some(texture);
        self.app.render_size = Some(render_size);
        self.app.device_scale_factor = Some(device_scale_factor);
        self.app.last_size = self.base().get_rect().size;
        self.app.last_dpi = initial_dpi;
    }

    fn create_browser(&mut self) {
        let logical_size = self.base().get_rect().size;
        let dpi = self.get_content_scale_factor();
        let pixel_width = (logical_size.x * dpi) as i32;
        let pixel_height = (logical_size.y * dpi) as i32;
        
        let window_info = WindowInfo {
            bounds: cef::Rect {
                x: 0 as _,
                y: 0 as _,
                width: pixel_width as _,
                height: pixel_height as _,
            },
            windowless_rendering_enabled: true as _,
            shared_texture_enabled: false as _,
            external_begin_frame_enabled: true as _,
            ..Default::default()
        };

        let browser_settings = BrowserSettings {
            ..Default::default()
        };

        let mut context = cef::request_context_create_context(
            Some(&RequestContextSettings::default()),
            Some(&mut webrender::RequestContextHandlerBuilder::build(webrender::OsrRequestContextHandler {})),
        );

        let render_handler = cef_app::OsrRenderHandler::new(
            dpi,
            PhysicalSize::new(pixel_width as f32, pixel_height as f32)
        );
        self.create_texture_and_buffer(&render_handler, dpi);
        
        let mut client = webrender::ClientBuilder::build(render_handler);

        let browser = cef::browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&self.url.to_string().as_str().into()),
            Some(&browser_settings),
            None,
            context.as_mut(),
        );

        assert!(browser.is_some(), "failed to create browser");

        self.app.browser = browser;
    }

    fn on_ready(&mut self) {
        self.base_mut().set_expand_mode(ExpandMode::IGNORE_SIZE);

        CEF_INITIALIZED.call_once(|| {
            Self::load_cef_framework();
            Self::initialize_cef();
        });

        self.create_browser();
        self.request_external_begin_frame();
    }

    fn on_process(&mut self) {
        self.handle_size_change();
        self.handle_dpi_change();

        if let Some(browser) = self.app.browser.as_mut() {
            if let Some(host) = browser.host() {
                host.send_external_begin_frame();
            }
        }

        run_message_loop();
        quit_message_loop();

        self.update_texture_from_buffer();
        self.request_external_begin_frame();
    }

    fn get_content_scale_factor(&self) -> f32 {
        if let Some(tree) = self.base().get_tree() {
            if let Some(window) = tree.get_root() {
                return window.get_content_scale_factor();
            }
        }
        1.0
    }

    fn handle_dpi_change(&mut self) {
        let current_dpi = self.get_content_scale_factor();
        if (current_dpi - self.app.last_dpi).abs() < 0.01 {
            return;
        }

        if let Some(device_scale_factor) = &self.app.device_scale_factor {
            if let Ok(mut dpi) = device_scale_factor.lock() {
                *dpi = current_dpi;
            }
        }

        // DPI change means physical pixel count changed, update render size
        let logical_size = self.base().get_rect().size;
        let pixel_width = logical_size.x * current_dpi;
        let pixel_height = logical_size.y * current_dpi;

        if let Some(render_size) = &self.app.render_size {
            if let Ok(mut size) = render_size.lock() {
                size.width = pixel_width;
                size.height = pixel_height;
            }
        }

        if let Some(browser) = self.app.browser.as_mut() {
            if let Some(host) = browser.host() {
                host.notify_screen_info_changed();
                host.was_resized();
            }
        }

        self.app.last_dpi = current_dpi;
    }

    fn handle_size_change(&mut self) {
        let logical_size = self.base().get_rect().size;
        if logical_size.x <= 0.0 || logical_size.y <= 0.0 {
            return;
        }

        // 1px tolerance to avoid resize loops
        let size_diff = (logical_size - self.app.last_size).abs();
        if size_diff.x < 1.0 && size_diff.y < 1.0 {
            return;
        }

        let dpi = self.get_content_scale_factor();
        let pixel_width = logical_size.x * dpi;
        let pixel_height = logical_size.y * dpi;

        if let Some(render_size) = &self.app.render_size {
            if let Ok(mut size) = render_size.lock() {
                size.width = pixel_width;
                size.height = pixel_height;
            }
        }

        if let Some(browser) = self.app.browser.as_mut() {
            if let Some(host) = browser.host() {
                host.was_resized();
            }
        }

        self.app.last_size = logical_size;
    }

    fn update_texture_from_buffer(&mut self) {
        let Some(frame_buffer_arc) = &self.app.frame_buffer else {
            return;
        };
        let Some(texture) = &mut self.app.texture else {
            return;
        };
        let Ok(mut frame_buffer) = frame_buffer_arc.lock() else {
            return;
        };
        if !frame_buffer.dirty || frame_buffer.data.is_empty() {
            return;
        }

        let width = frame_buffer.width as i32;
        let height = frame_buffer.height as i32;
        let byte_array = PackedByteArray::from(frame_buffer.data.as_slice());

        let image = Image::create_from_data(width, height, false, ImageFormat::RGBA8, &byte_array);
        if let Some(image) = image {
            texture.set_image(&image);
        }

        frame_buffer.mark_clean();
    }

    fn request_external_begin_frame(&mut self) {
        if let Some(browser) = self.app.browser.as_mut() {
            if let Some(host) = browser.host() {
                host.send_external_begin_frame();
            }
        }
    }

    fn handle_input_event(&mut self, event: Gd<InputEvent>) {
        if let Ok(mouse_button) = event.clone().try_cast::<InputEventMouseButton>() {
            self.handle_mouse_button_event(&mouse_button);
        } else if let Ok(mouse_motion) = event.try_cast::<InputEventMouseMotion>() {
            self.handle_mouse_motion_event(&mouse_motion);
        }
    }

    fn handle_mouse_button_event(&mut self, event: &Gd<InputEventMouseButton>) {
        let Some(browser) = self.app.browser.as_mut() else {
            return;
        };
        let Some(host) = browser.host() else {
            return;
        };

        let position = event.get_position();
        let dpi = self.get_content_scale_factor();
        let mouse_event = self.create_mouse_event(position, dpi, Self::get_modifiers_from_event(event));

        match event.get_button_index() {
            MouseButton::LEFT | MouseButton::MIDDLE | MouseButton::RIGHT => {
                let button_type = match event.get_button_index() {
                    MouseButton::LEFT => MouseButtonType::LEFT,
                    MouseButton::MIDDLE => MouseButtonType::MIDDLE,
                    MouseButton::RIGHT => MouseButtonType::RIGHT,
                    _ => unreachable!(),
                };
                let mouse_up = !event.is_pressed();
                let click_count = if event.is_double_click() { 2 } else { 1 };
                host.send_mouse_click_event(Some(&mouse_event), button_type, mouse_up as i32, click_count);
            }
            MouseButton::WHEEL_UP => {
                let factor = event.get_factor();
                let delta = (120.0 * factor) as i32;
                godot_print!("WHEEL_UP: factor={}, delta={}", factor, delta);
                host.send_mouse_wheel_event(Some(&mouse_event), 0, delta);
            }
            MouseButton::WHEEL_DOWN => {
                let factor = event.get_factor();
                let delta = (120.0 * factor) as i32;
                godot_print!("WHEEL_DOWN: factor={}, delta={}", factor, delta);
                host.send_mouse_wheel_event(Some(&mouse_event), 0, -delta);
            }
            MouseButton::WHEEL_LEFT => {
                let factor = event.get_factor();
                let delta = (120.0 * factor) as i32;
                godot_print!("WHEEL_LEFT: factor={}, delta={}", factor, delta);
                host.send_mouse_wheel_event(Some(&mouse_event), -delta, 0);
            }
            MouseButton::WHEEL_RIGHT => {
                let factor = event.get_factor();
                let delta = (120.0 * factor) as i32;
                godot_print!("WHEEL_RIGHT: factor={}, delta={}", factor, delta);
                host.send_mouse_wheel_event(Some(&mouse_event), delta, 0);
            }
            _ => {}
        }
    }

    fn handle_mouse_motion_event(&mut self, event: &Gd<InputEventMouseMotion>) {
        let Some(browser) = self.app.browser.as_mut() else {
            return;
        };
        let Some(host) = browser.host() else {
            return;
        };

        let position = event.get_position();
        let dpi = self.get_content_scale_factor();
        let mouse_event = self.create_mouse_event(position, dpi, Self::get_modifiers_from_motion_event(event));
        host.send_mouse_move_event(Some(&mouse_event), false as i32);
    }

    fn create_mouse_event(&self, position: Vector2, dpi: f32, modifiers: u32) -> MouseEvent {
        let x = (position.x * dpi) as i32;
        let y = (position.y * dpi) as i32;

        MouseEvent {
            x,
            y,
            modifiers,
        }
    }

    fn get_modifiers_from_event(event: &Gd<InputEventMouseButton>) -> u32 {
        let mut modifiers = cef_event_flags_t::EVENTFLAG_NONE.0;

        if event.is_shift_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0;
        }
        if event.is_ctrl_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0;
        }
        if event.is_alt_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_ALT_DOWN.0;
        }
        if event.is_meta_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0;
        }

        let button_mask = event.get_button_mask();
        if button_mask.is_set(MouseButtonMask::LEFT) {
            modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0;
        }
        if button_mask.is_set(MouseButtonMask::MIDDLE) {
            modifiers |= cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0;
        }
        if button_mask.is_set(MouseButtonMask::RIGHT) {
            modifiers |= cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0;
        }

        modifiers
    }

    fn get_modifiers_from_motion_event(event: &Gd<InputEventMouseMotion>) -> u32 {
        let mut modifiers = cef_event_flags_t::EVENTFLAG_NONE.0;

        if event.is_shift_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0;
        }
        if event.is_ctrl_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0;
        }
        if event.is_alt_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_ALT_DOWN.0;
        }
        if event.is_meta_pressed() {
            modifiers |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0;
        }

        let button_mask = event.get_button_mask();
        if button_mask.is_set(MouseButtonMask::LEFT) {
            modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0;
        }
        if button_mask.is_set(MouseButtonMask::MIDDLE) {
            modifiers |= cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0;
        }
        if button_mask.is_set(MouseButtonMask::RIGHT) {
            modifiers |= cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0;
        }

        modifiers
    }
}

static CEF_INITIALIZED: Once = Once::new();
