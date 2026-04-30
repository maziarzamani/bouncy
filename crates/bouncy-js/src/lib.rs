//! V8 isolate pool — phase 4 base.
//!
//! Tiny wrapper around `v8::OwnedIsolate` with a Mutex-protected free-list.
//! Each `eval` runs in a fresh Context so globals don't leak across pages.
//!
//! V8 process-wide initialization happens lazily on the first `IsolatePool`.

use std::sync::{Mutex, Once};

use thiserror::Error;

pub mod bridge;
mod runtime;
pub use runtime::Runtime;

/// V8 startup snapshot built from `js/bootstrap.js` at compile time.
/// Every isolate this crate mints loads this blob, so fresh contexts
/// inherit bouncy's polyfills without re-parsing JS.
static SNAPSHOT: &[u8] = include_bytes!(env!("BOUNCY_SNAPSHOT_PATH"));

#[derive(Error, Debug)]
pub enum Error {
    #[error("compile error: {0}")]
    Compile(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("conversion error")]
    Convert,

    /// `location.href = '...'` (and friends) halt the running script
    /// via V8 termination + a marker exception. Callers can distinguish
    /// this clean halt from a real runtime error and fold it into their
    /// nav-handling flow.
    #[error("execution halted by queued navigation")]
    NavTerminated,
}

static V8_INIT: Once = Once::new();

fn init_v8() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

pub struct IsolatePool {
    free: Mutex<Vec<v8::OwnedIsolate>>,
}

impl IsolatePool {
    pub fn new(size: usize) -> Self {
        init_v8();
        let mut free = Vec::with_capacity(size);
        for _ in 0..size {
            // Each isolate boots from the bootstrap snapshot; new
            // contexts inherit the polyfills set up there.
            let params = v8::CreateParams::default().snapshot_blob(v8::StartupData::from(SNAPSHOT));
            free.push(v8::Isolate::new(params));
        }
        Self {
            free: Mutex::new(free),
        }
    }

    pub fn checkout(&self) -> Option<IsolateGuard<'_>> {
        let isolate = self.free.lock().unwrap().pop()?;
        Some(IsolateGuard {
            pool: self,
            isolate: Some(isolate),
        })
    }
}

pub struct IsolateGuard<'a> {
    pool: &'a IsolatePool,
    isolate: Option<v8::OwnedIsolate>,
}

impl Drop for IsolateGuard<'_> {
    fn drop(&mut self) {
        if let Some(iso) = self.isolate.take() {
            self.pool.free.lock().unwrap().push(iso);
        }
    }
}

impl IsolateGuard<'_> {
    /// Compile + run `src` in a fresh Context. Returns the result coerced to
    /// a string.
    pub fn eval(&mut self, src: &str) -> Result<String, Error> {
        let isolate = self.isolate.as_mut().expect("guard active");
        v8::scope!(let handle, isolate);
        let context = v8::Context::new(handle, Default::default());
        let mut ctx_scope = v8::ContextScope::new(handle, context);
        v8::tc_scope!(let tc, &mut ctx_scope);

        let code = v8::String::new(tc, src).ok_or(Error::Convert)?;
        let script = match v8::Script::compile(tc, code, None) {
            Some(s) => s,
            None => {
                let msg = match tc.message() {
                    Some(m) => m.get(tc).to_rust_string_lossy(tc),
                    None => "<compile error>".into(),
                };
                return Err(Error::Compile(msg));
            }
        };
        let result = match script.run(tc) {
            Some(v) => v,
            None => {
                let msg = match tc.message() {
                    Some(m) => m.get(tc).to_rust_string_lossy(tc),
                    None => "<runtime error>".into(),
                };
                return Err(Error::Runtime(msg));
            }
        };
        let s = result
            .to_string(tc)
            .ok_or(Error::Convert)?
            .to_rust_string_lossy(tc);
        Ok(s)
    }
}
