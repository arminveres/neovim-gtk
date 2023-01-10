use std::{cell::RefCell, collections::HashMap, iter};

use log::{debug, error};

use nvim_rs::Value;

use crate::{
    nvim::{ErrorReport, NeovimApiInfo, NvimSession, SessionError},
    spawn_timeout,
};

/// Inner state struct for `Subscription`, we need this so that we can block/unblock `Subscription`s
/// from the callbacks of other `Subscription`s
#[derive(Default)]
struct SubscriptionState {
    /// The registered ID for this subscription's autocmd
    autocmd_id: Option<i64>,
    /// Whether or not this signal is blocked
    blocked: bool,
}

/// A subscription to a Neovim autocmd event.
struct Subscription {
    /// A callback to be executed each time the event triggers.
    cb: Box<dyn Fn(Vec<String>) + 'static>,
    /// A list of expressions which will be evaluated when the event triggers. The result is passed
    /// to the callback.
    args: Vec<String>,
    state: RefCell<SubscriptionState>,
}

impl Subscription {
    pub fn register(
        &self,
        nvim: &NvimSession,
        api_info: &NeovimApiInfo,
        key: &SubscriptionKey,
        idx: usize,
    ) -> Result<(), SessionError> {
        if self.state.borrow().blocked {
            return Ok(());
        }

        let i = idx.to_string();
        let args = iter::once(i.as_str())
            .chain(self.args.iter().map(|s| s.as_str()))
            .collect::<Box<[_]>>()
            .join(",");

        let autocmd_id = Some(nvim.block_on(nvim.timeout(nvim.create_autocmd(
            key.events.iter().cloned().map(|e| Value::from(e)).collect(),
            vec![
                    ("pattern".into(), key.pattern.as_str().into()),
                    (
                        "command".into(),
                        format!(
                            "cal rpcnotify(\
                                {id},\
                                'subscription',\
                                '{events}',\
                                '{pattern}',\
                                {args}\
                            )",
                            id = api_info.channel,
                            events = key.events.join(","),
                            pattern = key.pattern,
                        ).into()
                    )
                ],
        )))?);
        self.state.borrow_mut().autocmd_id = autocmd_id;

        Ok(())
    }

    /// Block the Subscription, and unregister it's autocmd if required.
    pub fn block(&self, nvim: &NvimSession) {
        let mut state = self.state.borrow_mut();
        if !state.blocked {
            if let Some(autocmd_id) = state.autocmd_id.take() {
                spawn_timeout!(nvim.del_autocmd(autocmd_id));
            }
            state.blocked = true;
        }
    }

    /// Unblock the Subscription, and register its autocmd if required.
    ///
    /// Returns whether or not we had to register its autocmd.
    pub fn unblock(
        &self,
        nvim: &NvimSession,
        api_info: &NeovimApiInfo,
        key: &SubscriptionKey,
        idx: usize,
    ) -> Result<bool, SessionError> {
        let mut state = self.state.borrow_mut();
        if state.blocked {
            state.blocked = false;
            drop(state);
            self.register(nvim, api_info, key, idx)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Subscription keys represent a NeoVim event coupled with a matching pattern. It is expected for
/// the pattern more often than not to be `"*"`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubscriptionKey {
    events: Vec<String>,
    pattern: String,
}

impl<T> From<T> for SubscriptionKey
where
    T: Into<String>,
{
    fn from(event_name: T) -> Self {
        SubscriptionKey {
            events: vec![event_name.into()],
            pattern: "*".to_owned(),
        }
    }
}

impl SubscriptionKey {
    pub fn new(events: &[&str]) -> Self {
        Self {
            events: events.iter().map(|s| s.to_string()).collect(),
            pattern: "*".to_string(),
        }
    }
    pub fn with_pattern(events: &[&str], pattern: &str) -> Self {
        Self {
            events: events.iter().map(|s| s.to_string()).collect(),
            pattern: pattern.to_owned(),
        }
    }
}

/// A map of all registered subscriptions.
pub struct Subscriptions(HashMap<SubscriptionKey, Vec<Subscription>>);

/// A handle to identify a `Subscription` within the `Subscriptions` map.
/// Can be used to trigger the subscription manually without the event needing to be triggered,
/// along with blocking event handling temporarily
#[derive(Debug, Clone)]
pub struct SubscriptionHandle {
    key: SubscriptionKey,
    index: usize,
}

impl Subscriptions {
    pub fn new() -> Self {
        Subscriptions(HashMap::new())
    }

    /// Subscribe to a Neovim autocmd event.
    ///
    /// Subscriptions are not active immediately but only after `set_autocmds` is called. At the
    /// moment, all calls to `subscribe` must be made before calling `set_autocmds`.
    ///
    /// This function is wrapped by `shell::State`.
    ///
    /// # Arguments:
    ///
    /// - `key`: The subscription key to register.
    ///   See `:help autocmd-events` for a list of supported event names. Event names can be
    ///   comma-separated.
    ///
    /// - `args`: A list of expressions to be evaluated when the event triggers.
    ///   Expressions are evaluated using Vimscript. The results are passed to the callback as a
    ///   list of Strings.
    ///   This is especially useful as `Neovim::eval` is synchronous and might block if called from
    ///   the callback function; so always use the `args` mechanism instead.
    ///
    /// - `cb`: The callback function.
    ///   This will be called each time the event triggers or when `run_now` is called.
    ///   It is passed a vector with the results of the evaluated expressions given with `args`.
    ///
    /// # Example
    ///
    /// Call a function each time a buffer is entered or the current working directory is changed.
    /// Pass the current buffer name and directory to the callback.
    /// ```
    /// let my_subscription = shell.state.borrow()
    ///     .subscribe("BufEnter,DirChanged", &["expand(@%)", "getcwd()"], move |args| {
    ///         let filename = &args[0];
    ///         let dir = &args[1];
    ///         // do stuff
    ///     });
    /// ```
    pub fn subscribe<F>(&mut self, key: SubscriptionKey, args: &[&str], cb: F) -> SubscriptionHandle
    where
        F: Fn(Vec<String>) + 'static,
    {
        let entry = self.0.entry(key.clone()).or_insert_with(Vec::new);
        let index = entry.len();
        entry.push(Subscription {
            cb: Box::new(cb),
            args: args.iter().map(|&s| s.to_owned()).collect(),
            state: RefCell::new(SubscriptionState {
                autocmd_id: None,
                blocked: false,
            }),
        });
        SubscriptionHandle { key, index }
    }

    /// Register all subscriptions with Neovim.
    ///
    /// This function is wrapped by `shell::State`.
    pub fn set_autocmds(
        &mut self,
        nvim: &NvimSession,
        api_info: &NeovimApiInfo,
    ) -> Result<(), SessionError> {
        for (key, subscriptions) in &mut self.0 {
            for (i, subscription) in subscriptions.iter_mut().enumerate() {
                subscription.register(nvim, api_info, key, i)?;
            }
        }
        Ok(())
    }

    /// Trigger given event.
    fn on_notify(&self, key: &SubscriptionKey, index: usize, args: Vec<String>) {
        /* It's possible we could receive an event for a blocked Subscription before nvim has
         * received our block, simply abort if this happens
         */
        if let Some(subscription) = self
            .0
            .get(key)
            .and_then(|v| v.get(index))
            .filter(|s| !s.state.borrow().blocked)
        {
            (*subscription.cb)(args);
        }
    }

    /// Wrapper around `on_notify` for easy calling with a `neovim_lib::Handler` implementation.
    ///
    /// This function is wrapped by `shell::State`.
    pub fn notify(&self, params: Vec<Value>) -> Result<(), String> {
        let mut params_iter = params.into_iter();
        let ev_name = params_iter.next();
        let ev_name = ev_name
            .as_ref()
            .and_then(Value::as_str)
            .ok_or("Error reading event name")?;
        let pattern = params_iter.next();
        let pattern = pattern
            .as_ref()
            .and_then(Value::as_str)
            .ok_or("Error reading pattern")?;
        let key = SubscriptionKey {
            events: ev_name.split(",").map(|e| e.to_string()).collect(),
            pattern: String::from(pattern),
        };
        let index = params_iter
            .next()
            .and_then(|i| i.as_u64())
            .ok_or("Error reading index")? as usize;
        let args = params_iter
            .map(|arg| {
                arg.as_str()
                    .map(str::to_owned)
                    .or_else(|| arg.as_u64().map(|uint| uint.to_string()))
            })
            .collect::<Option<Vec<String>>>()
            .ok_or("Error reading args")?;
        self.on_notify(&key, index, args);
        Ok(())
    }

    /// Manually trigger the given subscription.
    ///
    /// The `nvim` instance is needed to evaluate the `args` expressions.
    ///
    /// This function is wrapped by `shell::State`.
    pub fn run_now(&self, handle: &SubscriptionHandle, nvim: &NvimSession) {
        let subscription = &self.0.get(&handle.key).unwrap()[handle.index];
        let args = subscription
            .args
            .iter()
            .map(|arg| nvim.block_timeout(nvim.eval(arg)))
            .map(|res| {
                res.ok().and_then(|val| {
                    val.as_str()
                        .map(str::to_owned)
                        .or_else(|| val.as_u64().map(|uint: u64| format!("{uint}")))
                })
            })
            .collect::<Option<Vec<String>>>();
        if let Some(args) = args {
            self.on_notify(&handle.key, handle.index, args);
        } else {
            error!("Error manually running {:?}", handle);
        }
    }

    /// Prevent the given subscription from receiving events, and unregister its autocmd (if it has
    /// one) until `Subscriptions::unblock()` is called. If the subscription has no autocmd
    /// registered, this will prevent it from registering one until a subsequent unblock.
    pub fn block(&self, handle: &SubscriptionHandle, nvim: &NvimSession) {
        self.0.get(&handle.key).unwrap()[handle.index].block(nvim);
        debug!("Blocked {handle:?}");
    }

    /// Allow the given subscription receive events again, and re-register its autocmd if required.
    /// If we end up reregistering it's autocmd, we also execute the autocmd to bring whatever state
    /// it handles up to date.
    pub fn unblock(
        &self,
        handle: &SubscriptionHandle,
        nvim: &NvimSession,
        api_info: &NeovimApiInfo,
    ) {
        let subscription = &self.0.get(&handle.key).unwrap()[handle.index];

        if subscription
            .unblock(nvim, api_info, &handle.key, handle.index)
            .ok_and_report()
            == Some(true)
        {
            self.run_now(handle, nvim);
        }
        debug!("Unblocked {handle:?}");
    }
}
