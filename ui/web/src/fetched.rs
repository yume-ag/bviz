//! Documents bviz fetches by URL.

#[cfg(target_feature = "atomics")]
pub use crate::served::{Cancel, fetch};
use bviz_links::fetch::{Credentials, STALL};
#[cfg(not(target_feature = "atomics"))]
pub use single::{Cancel, fetch};
use web_sys::{
    ReferrerPolicy, RequestCache, RequestCredentials, RequestInit, RequestMode, RequestRedirect,
};

/// The options of every request for a document: a cross-origin read the
/// server must allow, telling it nothing of the page, and answered by the
/// server alone.
pub(crate) fn options(credentials: Credentials) -> RequestInit {
    let init = RequestInit::new();
    init.set_mode(RequestMode::Cors);
    init.set_credentials(match credentials {
        Credentials::Omit => RequestCredentials::Omit,
        Credentials::Include => RequestCredentials::Include,
    });
    init.set_referrer_policy(ReferrerPolicy::NoReferrer);
    init.set_cache(RequestCache::NoStore);
    init.set_redirect(if credentials.follows_redirects() {
        RequestRedirect::Follow
    } else {
        RequestRedirect::Error
    });
    init
}

/// [`STALL`] as a timeout of the browser's.
pub(crate) const STALL_MILLIS: i32 = STALL.as_millis() as i32;

/// A fetch read whole into memory on the window thread.
#[cfg(not(target_feature = "atomics"))]
mod single {
    use super::{STALL_MILLIS, options};
    use bviz_input::Named;
    use bviz_input::RESERVED_AHEAD_BYTES;
    use bviz_links::Origin;
    use bviz_links::fetch::{Answered, FetchError, Request};
    use js_sys::Uint8Array;
    use std::ops::ControlFlow;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        AbortController, ReadableStreamDefaultReader, ReadableStreamReadResult, Response, Window,
    };

    /// What cancels a fetch, which then fails as [`FetchError::Cancelled`].
    #[derive(Clone)]
    pub struct Cancel(AbortController);

    impl Cancel {
        pub fn cancel(&self) {
            self.0.abort_with_reason(&CANCELLED.into());
        }
    }

    /// The reasons a fetch is aborted with, by which a step that fails tells
    /// why.
    const STALLED: &str = "stalled";
    const CANCELLED: &str = "cancelled";

    /// What a step of the fetch `controller` aborts failing with `error`
    /// fails it with.
    fn failed(controller: &AbortController, error: JsValue) -> FetchError {
        match controller.signal().reason().as_string().as_deref() {
            Some(STALLED) => FetchError::Stalled,
            Some(CANCELLED) => FetchError::Cancelled,
            _ => FetchError::Failed(crate::js_message(error)),
        }
    }

    /// A fetch under way, which it aborts as it drops: once its document is
    /// read, to no effect.
    struct InFlight {
        controller: AbortController,
        window: Window,
        stall: Closure<dyn Fn()>,
        timer: i32,
    }

    impl InFlight {
        /// Gives the server another [`STALL`](bviz_links::fetch::STALL) to
        /// send its next bytes in.
        fn wait_again(&mut self) {
            self.window.clear_timeout_with_handle(self.timer);
            let timer = self
                .window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    self.stall.as_ref().unchecked_ref(),
                    STALL_MILLIS,
                );
            // A timer the browser does not set leaves the fetch to wait for
            // as long as the browser does.
            self.timer = timer.unwrap_or_default();
        }
    }

    impl Drop for InFlight {
        fn drop(&mut self) {
            self.window.clear_timeout_with_handle(self.timer);
            self.controller.abort();
        }
    }

    /// Fetches the document `request` asks for, reading it whole into
    /// memory once its response, coming from an origin other than the one
    /// asked, comes from one `accepts`. Tells `progress` how many bytes are
    /// read of how many the response declares, as each slice arrives, and
    /// fails as cancelled where `progress` breaks off the reading.
    pub fn fetch(
        request: Request,
        accepts: impl FnOnce(&Origin) -> bool,
        mut progress: impl FnMut(u64, Option<u64>) -> ControlFlow<()>,
    ) -> Result<(Cancel, impl Future<Output = Result<Named, FetchError>>), FetchError> {
        let window = web_sys::window().ok_or_else(|| FetchError::Failed("no window".into()))?;
        let controller =
            AbortController::new().map_err(|error| FetchError::Failed(crate::js_message(error)))?;
        let stalled = controller.clone();
        // A panic escaping the closure ends bviz in its crashed state, so
        // nothing observes what the unwind leaves behind.
        let stall = Closure::<dyn Fn()>::own_assert_unwind_safe(move || {
            stalled.abort_with_reason(&STALLED.into());
        });
        let mut in_flight = InFlight {
            controller: controller.clone(),
            window,
            stall,
            timer: 0,
        };
        let init = options(request.credentials);
        init.set_signal(Some(&controller.signal()));
        in_flight.wait_again();
        let answer = in_flight
            .window
            .fetch_with_str_and_init(request.url.url().as_str(), &init);
        let cancel = Cancel(controller);
        let fetched = async move {
            let response: Response = JsFuture::from(answer)
                .await
                .map_err(|error| failed(&in_flight.controller, error))?
                .unchecked_into();
            let length = declared_length(&response);
            let answered = Answered {
                url: &response.url(),
                status: response.status(),
                length,
            };
            request.check(&answered, accepts)?;
            let bytes = read_body(
                &response,
                length,
                request.max_bytes,
                &mut in_flight,
                |read| progress(read, length),
            )
            .await?;
            Ok(Named::Memory {
                name: request.url.name(),
                bytes,
            })
        };
        Ok((cancel, fetched))
    }

    /// The length `response` declares for its body.
    fn declared_length(response: &Response) -> Option<u64> {
        let length = response.headers().get("content-length").ok()??;
        bviz_links::fetch::declared_length(&length)
    }

    /// The body of `response`, of `length` bytes where that is known and at
    /// most `max`, in a buffer sized for it as [`Input::read_all`] sizes one.
    ///
    /// [`Input::read_all`]: bviz_input::Input::read_all
    async fn read_body(
        response: &Response,
        length: Option<u64>,
        max: u64,
        in_flight: &mut InFlight,
        mut progress: impl FnMut(u64) -> ControlFlow<()>,
    ) -> Result<Vec<u8>, FetchError> {
        let length = match length {
            Some(length) => Some(usize::try_from(length).map_err(|_| FetchError::OutOfMemory)?),
            None => None,
        };
        let mut bytes = Vec::new();
        if let Some(length) = length {
            let ahead = length.min(RESERVED_AHEAD_BYTES as usize);
            (bytes.try_reserve_exact(ahead)).map_err(|_| FetchError::OutOfMemory)?;
        }
        let Some(body) = response.body() else {
            return Ok(bytes);
        };
        let reader: ReadableStreamDefaultReader = body.get_reader().unchecked_into();
        loop {
            let read: ReadableStreamReadResult = JsFuture::from(reader.read())
                .await
                .map_err(|error| failed(&in_flight.controller, error))?
                .unchecked_into();
            if read.get_done() == Some(true) {
                return Ok(bytes);
            }
            in_flight.wait_again();
            let chunk = Uint8Array::new(&read.get_value());
            let start = bytes.len();
            let end = start + chunk.length() as usize;
            if end as u64 > max {
                return Err(FetchError::TooLarge {
                    declared: None,
                    max,
                });
            }
            let reserved = match length {
                Some(length) if end <= length => bytes.try_reserve_exact(length - start),
                _ => bytes.try_reserve(end - start),
            };
            reserved.map_err(|_| FetchError::OutOfMemory)?;
            bytes.resize(end, 0);
            chunk.copy_to(&mut bytes[start..]);
            if progress(end as u64).is_break() {
                return Err(FetchError::Cancelled);
            }
        }
    }
}
