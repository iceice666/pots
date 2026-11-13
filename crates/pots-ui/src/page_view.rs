//! The HTML page artifact's view: the page an agent wrote, running in a
//! native web view (WKWebView on macOS, WebKitGTK on Linux, WebView2 on
//! Windows) laid over the window where the editor would be.
//!
//! The page is untrusted, so the web view gets nothing from the app:
//! - no bridge: no IPC handler and no script injected into the page;
//! - no files: the page is served from a custom protocol out of memory,
//!   never from `file://`;
//! - no way out: navigating anywhere but the page, opening windows and
//!   downloads are refused;
//! - no network: the page is served with a Content-Security-Policy that
//!   allows only what is inline in it (see [`CSP`]);
//! - nothing kept: the web view's storage is ephemeral (incognito).
//!
//! People don't edit a page here: only agents write it. When one does, the
//! page reloads with the new HTML.
//!
//! Without GPUI Fast's window composition (Pots runs on gpui-pre), the web
//! view is a native view on top of the window: it covers anything GPUI
//! draws in its bounds, menus and dialogs included. The app hides it while
//! one of its own overlays is open (see `set_covered`).

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Window, div, prelude::*, px};
use gpui_webview::WebView;
use wry::http::{Request, Response, StatusCode, header};

use pots_agent::data::{ArtifactContent, Page};

use crate::artifact::{ArtifactEditor, EditorEvent};
use crate::theme;

/// The scheme the page is served from.
const SCHEME: &str = "potspage";

/// Where the page is: wry serves a custom scheme as `scheme://localhost/`,
/// except on Windows, where WebView2 needs it as `http://scheme.localhost/`.
#[cfg(not(windows))]
const PAGE_URL: &str = "potspage://localhost/";
#[cfg(windows)]
const PAGE_URL: &str = "http://potspage.localhost/";

/// What the page may use: only what is inline in it. Scripts and styles in
/// the page run; images, fonts and media only from `data:`/`blob:` URLs.
/// Nothing is fetched or sent (`fetch`, XHR, WebSocket, `<script src>`,
/// CDN fonts…), no frames or workers, and forms don't submit anywhere.
///
/// To let pages reach the network (load a library from a CDN, call an
/// API), this is where it would be loosened, e.g. `script-src
/// 'unsafe-inline' https:` and `connect-src https:`.
pub const CSP: &str = "default-src 'none'; \
                       script-src 'unsafe-inline'; \
                       style-src 'unsafe-inline'; \
                       img-src data: blob:; \
                       font-src data:; \
                       media-src data: blob:; \
                       connect-src 'none'; \
                       frame-src 'none'; \
                       child-src 'none'; \
                       worker-src 'none'; \
                       object-src 'none'; \
                       form-action 'none'; \
                       base-uri 'none'";

pub struct PageView {
    page: Page,
    /// What the custom protocol serves: the page as last shown.
    served: Arc<Mutex<String>>,
    /// How many times the page was reloaded, to give each load its own URL.
    loads: u64,
    /// Built on first draw, which has the window it goes in.
    webview: Option<Entity<WebView>>,
    /// Why there is no web view, if building it failed.
    error: Option<String>,
    /// An overlay of the app's is open over the page: the native view would
    /// cover it, so it is hidden meanwhile.
    covered: bool,
    focus: FocusHandle,
}

impl EventEmitter<EditorEvent> for PageView {}

impl PageView {
    pub fn new(page: Page, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let served = Arc::new(Mutex::new(page.html.clone()));
        Self { page, served, loads: 0, webview: None, error: None, covered: false, focus: cx.focus_handle() }
    }

    fn build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let served = self.served.clone();
        let builder = wry::WebViewBuilder::new()
            .with_custom_protocol(SCHEME.into(), move |_, request| serve(&served, &request))
            .with_url(PAGE_URL)
            .with_navigation_handler(|url| allowed(&url))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            // Dropping a file on the page would open it: refuse.
            .with_drag_drop_handler(|_| true)
            .with_incognito(true)
            .with_devtools(false)
            .with_clipboard(false)
            .with_autoplay(false)
            // Typing stays in the app until the page is clicked.
            .with_focused(false);
        match WebView::build(builder, window, cx) {
            Ok(webview) => {
                if self.covered {
                    let _ = webview.set_visible(false);
                }
                self.webview = Some(cx.new(|_| webview));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Load the page again, with the HTML now served.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.loads += 1;
        let url = format!("{PAGE_URL}?load={}", self.loads);
        if let Some(webview) = &self.webview {
            if let Err(e) = webview.read(cx).raw().load_url(&url) {
                eprintln!("pots: couldn't reload the page: {e}");
            }
        }
    }
}

/// The page, for a request to the custom protocol: only its root is there.
fn serve(served: &Mutex<String>, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let response = Response::builder().header(header::CACHE_CONTROL, "no-store");
    let response = match request.uri().path() {
        "/" | "" => {
            let html = served.lock().map(|h| h.clone()).unwrap_or_default();
            response
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CONTENT_SECURITY_POLICY, CSP)
                .header("X-Content-Type-Options", "nosniff")
                .body(Cow::Owned(html.into_bytes()))
        }
        _ => response.status(StatusCode::NOT_FOUND).body(Cow::Borrowed(&b""[..])),
    };
    response.expect("a valid response")
}

/// Whether the web view may go to `url`: only to the page itself (a
/// reload, or a link to an anchor in it).
fn allowed(url: &str) -> bool {
    url.strip_prefix(PAGE_URL).is_some_and(|rest| rest.is_empty() || rest.starts_with(['?', '#']))
}

impl ArtifactEditor for PageView {
    fn content(&self, _: &App) -> ArtifactContent {
        ArtifactContent::Page(self.page.clone())
    }

    fn has_pending_edit(&self, _: &App) -> bool {
        false
    }

    fn commit(&mut self, _: &mut Window, _: &mut Context<Self>) -> bool {
        false
    }

    /// The app keeps the keyboard: the page gets it when it is clicked.
    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    /// An agent rewrote the page: show the new one. What was typed into the
    /// old one is gone.
    fn replace(&mut self, content: ArtifactContent, cx: &mut Context<Self>) {
        let ArtifactContent::Page(page) = content else { return };
        if page == self.page {
            return;
        }
        if let Ok(mut served) = self.served.lock() {
            served.clone_from(&page.html);
        }
        self.page = page;
        if self.page.html.trim().is_empty() {
            // Dropping it takes the native view away.
            self.webview = None;
        } else {
            self.reload(cx);
        }
        cx.notify();
    }

    /// Hide the web view while something of the app's is drawn over it.
    fn set_covered(&mut self, covered: bool, cx: &mut Context<Self>) {
        if self.covered == covered {
            return;
        }
        self.covered = covered;
        if let Some(webview) = &self.webview {
            webview.update(cx, |w, _| {
                if let Err(e) = w.set_visible(!covered) {
                    eprintln!("pots: couldn't {} the page: {e}", if covered { "hide" } else { "show" });
                }
            });
        }
        cx.notify();
    }
}

impl Focusable for PageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.page.html.trim().is_empty();
        // Tests have no real window to put a web view in.
        if !empty && self.webview.is_none() && self.error.is_none() && cfg!(not(test)) {
            self.build(window, cx);
        }
        let note = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_size(px(14.))
                .text_color(theme::muted())
                .child(text)
        };
        let body = match (&self.webview, &self.error) {
            _ if empty => note("This page is empty. Ask an agent to write it.".into()).into_any_element(),
            (Some(webview), _) => webview.clone().into_any_element(),
            (None, Some(error)) => note(format!("The page can't be shown here: {error}")).into_any_element(),
            (None, None) => div().into_any_element(),
        };
        div().track_focus(&self.focus).size_full().child(body)
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use wry::http::Request;

    use super::*;

    #[test]
    fn only_the_page_itself_is_served_and_navigated_to() {
        let served = Mutex::new("<p>hi</p>".to_string());
        let page = serve(&served, &Request::get(PAGE_URL).body(Vec::new()).unwrap());
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(&page.body()[..], b"<p>hi</p>");
        assert_eq!(page.headers()[header::CONTENT_SECURITY_POLICY], CSP);
        let other = serve(&served, &Request::get(format!("{PAGE_URL}etc/passwd")).body(Vec::new()).unwrap());
        assert_eq!(other.status(), StatusCode::NOT_FOUND);
        assert!(other.body().is_empty());

        assert!(allowed(PAGE_URL));
        assert!(allowed(&format!("{PAGE_URL}?load=3")));
        assert!(allowed(&format!("{PAGE_URL}#top")));
        for url in [
            "https://example.com/",
            "file:///etc/passwd",
            "about:blank",
            "javascript:alert(1)",
            "data:text/html,<p>",
            &format!("{PAGE_URL}other"),
            "potspage://localhost.evil.com/",
            "http://potspage.localhost.evil.com/",
        ] {
            assert!(!allowed(url), "{url}");
        }
    }

    #[test]
    fn the_policy_lets_nothing_in_or_out() {
        let directives: Vec<&str> = CSP.split(';').map(str::trim).collect();
        assert_eq!(directives[0], "default-src 'none'");
        for d in ["connect-src 'none'", "frame-src 'none'", "form-action 'none'", "base-uri 'none'", "object-src 'none'"] {
            assert!(directives.contains(&d), "{d}");
        }
        assert!(!CSP.contains("http") && !CSP.contains('*'), "no remote source is allowed: {CSP}");
    }

    #[gpui::test]
    fn an_agents_page_replaces_the_one_shown(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let v = cx.new(|cx| PageView::new(Page { html: "<p>one</p>".into() }, window, cx));
            view = Some(v.clone());
            gpui_component::Root::new(v, window, cx)
        });
        let view = view.unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let page = |n: &str| ArtifactContent::Page(Page { html: n.into() });
        view.update(cx, |v, cx| v.replace(page("<p>two</p>"), cx));
        view.read_with(cx, |v, cx| {
            assert_eq!(v.content(cx), page("<p>two</p>"));
            assert_eq!(*v.served.lock().unwrap(), "<p>two</p>", "the protocol serves the new page");
            assert_eq!(v.loads, 1);
        });
        view.update(cx, |v, cx| v.replace(page("<p>two</p>"), cx));
        assert_eq!(view.read_with(cx, |v, _| v.loads), 1, "the same page isn't reloaded");
        view.update(cx, |v, cx| v.set_covered(true, cx));
        assert!(view.read_with(cx, |v, _| v.covered));
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
}
