//! The application context every command runs against: the configuration,
//! the `Ui`, and the sources built from that configuration.
//!
//! It lives in a module of its own so that `config` is private to this file.
//! Every read goes through [`AppContext::config`] and every write through
//! [`AppContext::config_mut`] or [`AppContext::set_config`], which discard the
//! sources, so nothing built from an earlier snapshot is handed out after a
//! write.

/*-- public --*/

pub(crate) struct AppContext {
    config: crate::config::Config,
    pub(crate) ui: std::sync::Arc<dyn crate::utils::ui::Ui>,
    /// The sources built from `config`, built on the first ask and dropped
    /// whenever configuration is written. Behind a `Mutex` because commands
    /// ask for them through `&self` and hold what they get across await
    /// points.
    sources: std::sync::Mutex<Option<crate::sources::Sources>>,
    /// The session proxy for this run, when a launch started one. Every
    /// provider the sources hand out points at it, which is why setting it
    /// discards them.
    model_proxy: Option<crate::proxy::ProxyHandle>,
}

impl AppContext {
    pub(crate) fn new(
        config: crate::config::Config,
        ui: std::sync::Arc<dyn crate::utils::ui::Ui>,
    ) -> Self {
        Self {
            config,
            ui,
            sources: std::sync::Mutex::new(None),
            model_proxy: None,
        }
    }

    /// The configuration as it now stands.
    pub(crate) fn config(&self) -> &crate::config::Config {
        &self.config
    }

    /// The configuration, to change. Taking it discards the sources, so
    /// nothing built from the old snapshot is handed out after a write.
    pub(crate) fn config_mut(&mut self) -> &mut crate::config::Config {
        self.invalidate();
        &mut self.config
    }

    /// Replace the configuration wholesale, which a launch does to pick up
    /// whatever was saved before it ran.
    pub(crate) fn set_config(&mut self, config: crate::config::Config) {
        self.invalidate();
        self.config = config;
    }

    /// Point every provider the sources hand out at the session proxy a
    /// launch just started.
    pub(crate) fn set_model_proxy(&mut self, model_proxy: crate::proxy::ProxyHandle) {
        self.invalidate();
        self.model_proxy = Some(model_proxy);
    }

    /// The sources for the configuration as it now stands, built on the
    /// first ask. The handles outlive the snapshot they came from, so a
    /// launch keeps the set it resolved against for as long as it runs.
    pub(crate) fn sources(&self) -> crate::sources::Sources {
        let mut held = self.sources.lock().unwrap();
        held.get_or_insert_with(|| {
            crate::sources::Sources::build(&self.config, self.model_proxy.clone())
        })
        .clone()
    }

    fn invalidate(&mut self) {
        *self.sources.lock().unwrap() = None;
    }
}
