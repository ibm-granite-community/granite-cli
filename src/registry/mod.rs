mod secret;
pub use secret::Secret;

/*-- Generic Factory Infrastructure ------------------------------------------*/

/// Unit struct for types that have no structured config.
/// Used by test doubles and impls that genuinely have no config to declare.
#[derive(schemars::JsonSchema, serde::Serialize, serde::Deserialize, Default)]
pub struct NoConfig {}

/// Why a factory could not produce an instance.
///
/// The two cases are kept apart so a caller can act on them without reading
/// the message: an unregistered type name is a different problem, with a
/// different repair, from settings that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstructError {
    /// The name given is not a key in this kind's registry.
    UnknownType { type_name: String },
    /// The settings blob does not produce an instance of this type, because
    /// it does not deserialise into the type's `Config` or because building
    /// from it failed.
    Settings { detail: String },
}

impl ConstructError {
    /// Report settings that cannot be read. `detail` is whatever the
    /// deserialiser or the builder said; the caller that names the instance
    /// adds the rest.
    pub fn settings(detail: impl std::fmt::Display) -> Self {
        Self::Settings {
            detail: detail.to_string(),
        }
    }
}

impl ConstructError {
    /// This failure as a message naming the instance it is about, for a
    /// source that knows which kind and id it was asked for. One wording for
    /// all four kinds, so the same problem reads the same whichever source
    /// reports it.
    pub fn about(&self, kind: &str, instance_id: &str) -> anyhow::Error {
        match self {
            Self::UnknownType { type_name } => {
                anyhow::anyhow!("{kind} '{instance_id}' has an unknown {kind} type '{type_name}'")
            }
            Self::Settings { detail } => {
                anyhow::anyhow!("the settings for {kind} '{instance_id}' are not valid: {detail}")
            }
        }
    }
}

impl std::fmt::Display for ConstructError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownType { type_name } => write!(f, "unknown instance type: {type_name}"),
            Self::Settings { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ConstructError {}

/// Core trait that all factory-managed types must implement.
/// Provides construction from a configuration object.
pub trait ConfigConstructable {
    /// The structured config type for this implementation.
    /// Must implement `JsonSchema + Serialize + Default`.
    type Config: schemars::JsonSchema + serde::Serialize + Default;

    /// Construct with the instance's configured name and its own config.
    ///
    /// `instance_id` is the key this instance is configured under (e.g. the
    /// provider nickname `my-ollama`), *not* the registry type name. Implementations
    /// that need to identify themselves downstream store it and surface it via
    /// [`Named`]. For instances constructed outside any configured set (bare
    /// catalog lookups, `--output` backends), callers pass the type name.
    ///
    /// A name this instance refers to is resolved after construction, by the
    /// source that owns what is being named, so nothing here needs the
    /// application configuration.
    ///
    /// Reading `cfg` is the one thing that can fail. An implementation
    /// deserialises it into its own `Config` and reports
    /// [`ConstructError::Settings`] when that does not work, so a blob that
    /// does not parse is named instead of silently becoming a default.
    /// Construction does no I/O, so nothing else here has anything to report.
    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError>
    where
        Self: Sized;
}

/// Lets a factory-constructed instance report the configured name it was built
/// for. Object-safe on purpose: the domain traits (`Provider`, `Model`,
/// `Capability`, `Launcher`) require it, so a `&dyn Provider` can answer "which
/// configured provider are you?" without downcasting.
///
/// The value is whatever `instance_id` was passed to
/// [`ConfigConstructable::new`], so it round-trips the config key that produced
/// the instance.
pub trait Named {
    fn instance_id(&self) -> &str;
}

/// Macro to define a complete factory infrastructure for a trait hierarchy.
///
/// This macro generates:
/// - An internal metadata trait for type erasure
/// - A HasMetadata trait that implementations must provide
/// - A MetaOf wrapper for connecting implementations to metadata
/// - A Factory struct with registration and construction capabilities
///
/// # Arguments
///
/// * `$trait` - The trait being factored (e.g., Provider, Capability)
/// * `$config` - The config type used for construction
/// * `$metadata` - The metadata type returned by describe()
/// * `$factory` - Name for the Factory struct
///
/// # Example
///
/// ```ignore
/// trait MyTrait: ConfigConstructable {
///     fn do_something(&self);
/// }
/// struct MyMetadata { value: i32 }
///
/// define_factory!(
///     MyTrait,
///     MyMetadata,
///     MyTraitFactory
/// );
///
/// struct MySomething { value: i32 }
/// impl MyTrait for MySomething {
///     fn do_something(&self) { println!("My value: {}", self.value); }
/// }
/// impl HasMyTraitMetadata for MySomething {
///     fn metadata() -> String { "I belong to you".to_string() }
/// }
/// ```
#[macro_export]
macro_rules! define_factory {
    ($trait:ident, $metadata:ty, $factory:ident) => {
        /// Wrapper type that connects an implementation to its metadata.
        /// Uses PhantomData to maintain type information without storing instances.
        struct MetaOf<T>(std::marker::PhantomData<T>);
        impl<T> MetaOf<T> {
            const fn new() -> Self {
                Self(std::marker::PhantomData)
            }
        }

        $crate::paste::paste! {
            /// Internal trait for metadata provision and construction.
            /// This trait enables type erasure while maintaining type safety.
            /// The `#[allow(unused)]` annotations below are for the `Ui`
            /// factory, the one instantiation nothing describes or prompts
            /// for: `--output` constructs a backend by name and never asks
            /// for its metadata, schema or defaults outside tests.
            pub(crate) trait [<$trait Metadata_>]: Send + Sync {
                /// Get metadata describing this implementation
                #[allow(unused)]
                fn describe(&self) -> $metadata;

                /// Construct an instance with its configured name and config
                fn construct(
                    &self,
                    instance_id: &str,
                    cfg: &serde_json::Value,
                ) -> Result<Box<dyn $trait>, $crate::registry::ConstructError>;

                /// JSON schema of the config this implementation expects
                #[allow(unused)]
                fn config_schema(&self) -> schemars::Schema;

                /// Default config value for this implementation
                #[allow(unused)]
                fn default_config(&self) -> serde_json::Value;
            }

            /// Trait that implementations must provide to supply metadata.
            /// This is the public interface for implementations to describe themselves.
            pub trait [<Has $trait Metadata>] {
                /// Return metadata describing this implementation
                fn metadata() -> $metadata;
            }

            /// Implementation of the internal metadata trait for any type T
            /// that implements the required traits.
            impl<T> [<$trait Metadata_>] for MetaOf<T>
            where
                T: $trait
                    + [<Has $trait Metadata>]
                    + ConfigConstructable<Config: schemars::JsonSchema + serde::Serialize + Default>
                    + Send
                    + Sync
                    + 'static,
            {
                fn describe(&self) -> $metadata {
                    T::metadata()
                }

                fn construct(
                    &self,
                    instance_id: &str,
                    cfg: &serde_json::Value,
                ) -> Result<Box<dyn $trait>, $crate::registry::ConstructError> {
                    Ok(Box::new(T::new(instance_id, cfg)?))
                }

                fn config_schema(&self) -> schemars::Schema {
                    schemars::schema_for!(<T as ConfigConstructable>::Config)
                }

                fn default_config(&self) -> serde_json::Value {
                    serde_json::to_value(<T as ConfigConstructable>::Config::default())
                        .unwrap_or_default()
                }
            }

            /// Factory for creating and managing instances of the trait.
            ///
            /// The factory maintains a registry of implementations and provides
            /// methods to:
            /// - Register new implementations
            /// - Construct instances by name
            /// - Query metadata
            /// - List all registered implementations
            pub struct $factory {
                registry: std::collections::HashMap<&'static str, Box<dyn [<$trait Metadata_>]>>,
            }

            impl $factory {
                /// Create a new empty factory
                pub(crate) fn new() -> Self {
                    Self {
                        registry: std::collections::HashMap::new(),
                    }
                }

                /// Register an implementation with the given name.
                ///
                /// # Type Parameters
                ///
                /// * `T` - The implementation type to register
                ///
                /// # Arguments
                ///
                /// * `name` - Static string identifier for this implementation
                pub(crate) fn register<T>(&mut self, name: &'static str)
                where
                    T: $trait
                        + ConfigConstructable<
                            Config: schemars::JsonSchema + serde::Serialize + Default,
                        > + [<Has $trait Metadata>]
                        + Send
                        + Sync
                        + 'static,
                {
                    self.registry.insert(name, Box::new(MetaOf::<T>::new()));
                }

                /// Construct an instance by name with the given configuration.
                ///
                /// `name` selects the registered implementation. `instance_id`
                /// and `cfg` go to that implementation's
                /// `ConfigConstructable::new` unchanged, so `cfg` is what decides
                /// the instance:
                /// - A saved instance's config produces that configured instance,
                ///   and `instance_id` is the config key it came from.
                /// - A default or ad-hoc config produces an ephemeral instance of
                ///   a type that is not configured, built to inspect it. The API
                ///   expects `name` as the value of `instance_id` in this case,
                ///   since there is no config key.
                ///
                /// `instance_id` is only the label the instance reports through
                /// [`Named`]. Nothing here looks it up, so an id that is not
                /// configured constructs the same as one that is.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation to construct
                /// * `instance_id` - The configured name of this instance (the config
                ///   key it came from). Pass `name` for instances that aren't drawn
                ///   from a configured set.
                /// * `cfg` - Configuration to pass to the constructor
                ///
                /// # Returns
                ///
                /// * `Ok(Box<dyn Trait>)` - Successfully constructed instance
                /// * `Err(ConstructError::UnknownType)` - `name` is not registered
                /// * `Err(ConstructError::Settings)` - `cfg` cannot be read as
                ///   this type's config
                pub(crate) fn construct(
                    &self,
                    name: &str,
                    instance_id: &str,
                    cfg: &serde_json::Value,
                ) -> Result<Box<dyn $trait>, $crate::registry::ConstructError> {
                    self.registry
                        .get(name)
                        .ok_or_else(|| $crate::registry::ConstructError::UnknownType {
                            type_name: name.to_string(),
                        })?
                        .construct(instance_id, cfg)
                }

                /// Get metadata for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(metadata)` - Metadata if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn get(&self, name: &str) -> Option<$metadata> {
                    self.registry.get(name).map(|x| x.describe())
                }

                /// Get all registered implementations with their metadata.
                ///
                /// # Returns
                ///
                /// HashMap mapping names to metadata for all registered implementations
                #[allow(unused)]
                pub(crate) fn entries(&self) -> std::collections::HashMap<&str, $metadata> {
                    self.registry
                        .iter()
                        .map(|(k, v)| (*k, v.describe()))
                        .collect()
                }

                /// Get the config JSON schema for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(schema)` - Schema of the config `construct` expects, if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn config_schema(&self, name: &str) -> Option<schemars::Schema> {
                    self.registry.get(name).map(|x| x.config_schema())
                }

                /// Get the default config value for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(value)` - Default config value, if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn default_config(&self, name: &str) -> Option<serde_json::Value> {
                    self.registry.get(name).map(|x| x.default_config())
                }
            }
        }

        impl Default for $factory {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::dependency::Catalogued for dyn $trait {
            type Metadata = $metadata;
        }
    };
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    // Hoist paste macro for use in the macro-expanded traits
    extern crate paste;

    // Test trait and types
    pub(crate) trait TestTrait: Named {
        fn get_value(&self) -> i32;
    }

    // Define factory for test trait (3 params: trait, metadata type, factory name)
    define_factory!(TestTrait, String, TestTraitFactory);

    // Test implementation 1
    struct TestImpl1 {
        instance_id: String,
        value: i32,
    }

    impl ConfigConstructable for TestImpl1 {
        type Config = NoConfig;

        fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
            let value = cfg.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            Ok(Self {
                instance_id: instance_id.to_string(),
                value,
            })
        }
    }

    impl TestTrait for TestImpl1 {
        fn get_value(&self) -> i32 {
            self.value
        }
    }

    impl Named for TestImpl1 {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    impl HasTestTraitMetadata for TestImpl1 {
        fn metadata() -> String {
            "TestImpl1: A test implementation".to_string()
        }
    }

    // Test implementation 2
    struct TestImpl2 {
        instance_id: String,
        value: i32,
    }

    impl ConfigConstructable for TestImpl2 {
        type Config = TestImpl2Config;

        fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
            let value = cfg.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            Ok(Self {
                instance_id: instance_id.to_string(),
                value: value * 2,
            })
        }
    }

    impl TestTrait for TestImpl2 {
        fn get_value(&self) -> i32 {
            self.value
        }
    }

    impl Named for TestImpl2 {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[derive(schemars::JsonSchema, serde::Serialize, Default)]
    struct TestImpl2Config {
        #[allow(unused)] // Used for schema inspection
        value: i32,
    }

    impl HasTestTraitMetadata for TestImpl2 {
        fn metadata() -> String {
            "TestImpl2: Another test implementation".to_string()
        }
    }

    /// A type whose config has a typed field, so a blob of the wrong shape
    /// is something `new` can report.
    struct TestImpl3 {
        instance_id: String,
        value: i32,
    }

    #[derive(schemars::JsonSchema, serde::Serialize, serde::Deserialize, Default)]
    struct TestImpl3Config {
        value: i32,
    }

    impl ConfigConstructable for TestImpl3 {
        type Config = TestImpl3Config;

        fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
            let config: TestImpl3Config =
                serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
            Ok(Self {
                instance_id: instance_id.to_string(),
                value: config.value,
            })
        }
    }

    impl TestTrait for TestImpl3 {
        fn get_value(&self) -> i32 {
            self.value
        }
    }

    impl Named for TestImpl3 {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    impl HasTestTraitMetadata for TestImpl3 {
        fn metadata() -> String {
            "TestImpl3: settings with a typed field".to_string()
        }
    }

    #[test]
    fn test_factory_registration() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        assert!(factory.get("impl1").is_some());
        assert!(factory.get("impl2").is_some());
        assert!(factory.get("impl3").is_none());
    }

    #[test]
    fn test_factory_metadata() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let meta1 = factory.get("impl1").unwrap();
        assert!(meta1.contains("TestImpl1"));

        let meta2 = factory.get("impl2").unwrap();
        assert!(meta2.contains("TestImpl2"));
    }

    #[test]
    fn test_factory_construction() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let cfg = serde_json::json!({ "value": 42 });

        let inst1 = factory.construct("impl1", "my-impl1", &cfg).unwrap();
        assert_eq!(inst1.get_value(), 42);

        let inst2 = factory.construct("impl2", "my-impl2", &cfg).unwrap();
        assert_eq!(inst2.get_value(), 84); // TestImpl2 doubles the value
    }

    #[test]
    fn test_factory_construct_unknown() {
        let factory = TestTraitFactory::new();
        let cfg = serde_json::json!({ "value": 42 });

        let result = factory.construct("unknown", "unknown", &cfg);
        assert_eq!(
            result.err().unwrap(),
            ConstructError::UnknownType {
                type_name: "unknown".to_string()
            }
        );
    }

    #[test]
    fn construct_reports_settings_it_cannot_read() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl3>("impl3");

        let err = factory
            .construct(
                "impl3",
                "my-impl3",
                &serde_json::json!({ "value": "not a number" }),
            )
            .err()
            .unwrap();
        assert!(
            matches!(&err, ConstructError::Settings { detail } if detail.contains("invalid type")),
            "expected unreadable settings, got {err:?}"
        );
    }

    #[test]
    fn construct_accepts_a_key_it_does_not_know() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl3>("impl3");

        // A configuration written by a later version carries keys this one
        // has never heard of, and still names an instance this one can build.
        let inst = factory
            .construct(
                "impl3",
                "my-impl3",
                &serde_json::json!({ "value": 7, "from_the_future": true }),
            )
            .unwrap();
        assert_eq!(inst.get_value(), 7);
    }

    #[test]
    fn test_factory_entries() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let entries = factory.entries();
        assert_eq!(entries.len(), 2);
        let metadata_strs: Vec<String> = entries.into_values().collect();
        assert!(metadata_strs.iter().any(|s| s.contains("TestImpl1")));
        assert!(metadata_strs.iter().any(|s| s.contains("TestImpl2")));
    }

    #[test]
    fn test_factory_default() {
        let factory = TestTraitFactory::default();
        assert_eq!(factory.entries().len(), 0);
    }

    #[test]
    fn test_config_schema_default_is_opaque() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");

        // TestImpl1 uses NoConfig, which produces a proper schema for an empty object type.
        let schema = factory.config_schema("impl1").unwrap();
        assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"));
        assert_eq!(
            schema.get("title").and_then(|t| t.as_str()),
            Some("NoConfig")
        );
    }

    #[test]
    fn test_config_schema_uses_override() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl2>("impl2");

        let schema = factory.config_schema("impl2").unwrap();
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema with properties");
        assert!(properties.contains_key("value"));
    }

    #[test]
    fn test_config_schema_unknown() {
        let factory = TestTraitFactory::new();
        assert!(factory.config_schema("unknown").is_none());
    }

    #[test]
    fn test_default_config_default_is_empty_object() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");

        // TestImpl1 uses NoConfig, which serializes to an empty object.
        let value = factory.default_config("impl1").unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn test_default_config_uses_override() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl2>("impl2");

        let value = factory.default_config("impl2").unwrap();
        assert_eq!(value, serde_json::json!({ "value": 0 }));
    }

    #[test]
    fn test_default_config_unknown() {
        let factory = TestTraitFactory::new();
        assert!(factory.default_config("unknown").is_none());
    }
}
