//! Generic type-to-type dependency resolution.
//!
//! A type `T` (typically an in-progress configuration choice, e.g. "set up
//! this model with this variant") can depend on a configured instance of some
//! other trait hierarchy `U` (e.g. `dyn Provider`). This module defines the
//! shared machinery for answering, generically over any such pair:
//!
//!   1. Type-level: could a *kind* of `U` registered in its catalog ever
//!      satisfy this dependency? (checked against catalog metadata, before
//!      any instance exists)
//!   2. Instance-level: does an already-configured, live `U` actually
//!      satisfy it?
//!
//! Concrete pairings (e.g. Model -> Provider) implement `Requirement` and
//! `DependsOn`; this module only provides the generic traits and the resolver
//! that operates on them.

use std::collections::HashMap;
use std::sync::Arc;

/*-- Catalogued ----------------------------------------------------------------*/

/// Binds a trait hierarchy to the metadata type its factory catalogs
/// implementations with. Implemented automatically by `define_factory!` for
/// every `$trait` it defines, via `impl Catalogued for dyn $trait`.
pub trait Catalogued {
    type Metadata;
}

/*-- Requirement ----------------------------------------------------------------*/

/// Something that knows what it needs from a `U`, and can judge candidates.
///
/// The type-level and instance-level checks are paired on one trait so they
/// can't drift apart: `admits_type` exists only to narrow the catalog before
/// anything is constructed, and should never accept a metadata entry whose
/// instances could never pass `admits_instance`.
pub trait Requirement<U: Catalogued + ?Sized> {
    /// Type-level (catalog) check: could an instance built from this catalog
    /// entry possibly satisfy the requirement?
    fn admits_type(&self, metadata: &U::Metadata) -> bool;

    /// Instance-level check: does this already-configured, live instance
    /// actually satisfy the requirement?
    fn admits_instance(&self, instance: &U) -> bool;
}

/*-- DependsOn ------------------------------------------------------------------*/

/// Declares that `T` has a dependency slot on `U`, filled by producing a
/// `Requirement`.
///
/// `T` is usually not the static trait/catalog type itself, but whatever
/// captures the in-progress choice driving the requirement -- the
/// requirement often depends on runtime state the catalog metadata alone
/// doesn't carry (e.g. which variant of a model was picked).
pub trait DependsOn<U: Catalogued + ?Sized> {
    type Requirement: Requirement<U>;

    fn requirement(&self) -> Self::Requirement;
}

/*-- Configured -----------------------------------------------------------------*/

/// Adapts a dependency's world -- its already-configured instances, and its
/// type-level catalog -- so the generic resolver can search both without
/// knowing anything about `U`'s concrete storage or construction.
pub trait Configured<U: Catalogued + ?Sized> {
    /// Already-configured instances, keyed by their configured id.
    /// Returns `Arc` clones so callers can hold shared ownership without
    /// borrowing from the source.
    fn instances(&self) -> Vec<(String, Arc<U>)>;

    /// Registered catalog types (type-level metadata), keyed by registry name.
    fn catalog(&self) -> HashMap<&'static str, U::Metadata>;

    /// JSON schema of the config a catalog type name expects to be
    /// constructed with. Callers use this to drive the next round of
    /// configuration for one of a `Resolution`'s `configurable_types`,
    /// without knowing anything about that type's concrete config struct.
    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema>;
}

/*-- Resolution -----------------------------------------------------------------*/

/// Outcome of resolving a `Requirement` against a `Configured<U>` source.
///
/// Both facets are always populated; it's up to the caller (typically an
/// interactive setup flow) to decide how to present them -- e.g. offer the
/// existing instances as ready-to-use choices, plus an "configure a new one"
/// option backed by `configurable_types` when it's non-empty. The dependency
/// is unresolvable only when both are empty.
#[derive(Debug)]
pub struct Resolution {
    /// Already-configured instances that satisfy the requirement, keyed by
    /// their configured id.
    pub existing_instances: Vec<String>,
    /// Catalog type names that could satisfy the requirement if configured.
    pub configurable_types: Vec<&'static str>,
}

impl Resolution {
    /// True when nothing configured satisfies the requirement, and nothing in
    /// the catalog could ever be configured to satisfy it either.
    pub fn is_unsatisfiable(&self) -> bool {
        self.existing_instances.is_empty() && self.configurable_types.is_empty()
    }
}

/*-- ModelConfigured -----------------------------------------------------------*/

/// Extension of `Configured<dyn Model>` that can resolve a live `Provider`
/// for a model instance using the source's own provider config map.
///
/// This keeps provider resolution at call time (not baked into the model
/// struct at construction time) without breaking the `Model::provider()`
/// trait signature.
pub trait ModelConfigured: Configured<dyn crate::models::Model> + Send + Sync {
    /// Construct a `Provider` for `model` using this source's live provider
    /// config map. Returns an error if the model has no provider id or the
    /// referenced provider is not in the config map.
    fn provider_for(
        &self,
        model: &dyn crate::models::Model,
    ) -> anyhow::Result<Box<dyn crate::providers::Provider>>;
}

/*-- Resolution ----------------------------------------------------------------*/

/// Resolve `T`'s dependency on `U` against a `Configured<U>` source.
///
/// Pure predicate evaluation -- no I/O, no prompting. Callers turn a
/// `Resolution` into behavior: auto-select a single existing match, prompt
/// among several, or drive the setup flow for one of the configurable types
/// (which may itself recurse into further unresolved dependencies).
pub fn resolve<T, U>(dependent: &T, source: &impl Configured<U>) -> Resolution
where
    U: Catalogued + ?Sized,
    T: DependsOn<U>,
{
    let requirement = dependent.requirement();

    let existing_instances: Vec<String> = source
        .instances()
        .into_iter()
        .filter(|(_, instance)| requirement.admits_instance(instance.as_ref()))
        .map(|(id, _)| id)
        .collect();

    let configurable_types: Vec<&'static str> = source
        .catalog()
        .into_iter()
        .filter(|(_, metadata)| requirement.admits_type(metadata))
        .map(|(name, _)| name)
        .collect();

    Resolution {
        existing_instances,
        configurable_types,
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    // Toy `U` hierarchy: a can of paint. Catalog metadata claims which colors
    // a *kind* of paint can be mixed to; a live instance has already settled
    // on one actual color.
    trait Paint {
        fn color(&self) -> &str;
    }

    struct PaintMetadata {
        claimed_colors: Vec<&'static str>,
    }

    impl Catalogued for dyn Paint {
        type Metadata = PaintMetadata;
    }

    struct MixedPaint(&'static str);
    impl Paint for MixedPaint {
        fn color(&self) -> &str {
            self.0
        }
    }

    // Toy `T`: something that wants a paint of a specific color.
    struct WantsColor(&'static str);

    struct ColorRequirement(&'static str);
    impl Requirement<dyn Paint> for ColorRequirement {
        fn admits_type(&self, metadata: &PaintMetadata) -> bool {
            metadata.claimed_colors.contains(&self.0)
        }
        fn admits_instance(&self, instance: &dyn Paint) -> bool {
            instance.color() == self.0
        }
    }

    impl DependsOn<dyn Paint> for WantsColor {
        type Requirement = ColorRequirement;
        fn requirement(&self) -> ColorRequirement {
            ColorRequirement(self.0)
        }
    }

    // A recipe's config schema, in real usage, would come from the concrete
    // mixing type's own `HasPaintMetadata::config_schema` override -- e.g.
    // `schemars::schema_for!(CyanMixConfig)`. The toy shop stores one
    // directly per recipe to stand in for that per-type lookup.
    #[derive(schemars::JsonSchema)]
    struct MixRatio {
        #[allow(unused)] // Used for schema inspection
        parts_base: u32,
    }

    // Toy `Configured<dyn Paint>` source: a fixed set of already-mixed cans,
    // plus a catalog of "recipes" that could be mixed on demand.
    struct PaintShop {
        cans: Vec<(String, Arc<dyn Paint>)>,
        recipes: HashMap<&'static str, PaintMetadata>,
        recipe_schemas: HashMap<&'static str, schemars::Schema>,
    }

    impl Configured<dyn Paint> for PaintShop {
        fn instances(&self) -> Vec<(String, Arc<dyn Paint + 'static>)> {
            self.cans
                .iter()
                .map(|(id, p)| (id.clone(), Arc::clone(p)))
                .collect()
        }
        fn catalog(&self) -> HashMap<&'static str, PaintMetadata> {
            self.recipes
                .iter()
                .map(|(name, meta)| {
                    (
                        *name,
                        PaintMetadata {
                            claimed_colors: meta.claimed_colors.clone(),
                        },
                    )
                })
                .collect()
        }
        fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
            self.recipe_schemas.get(type_name).cloned()
        }
    }

    fn empty_shop() -> PaintShop {
        PaintShop {
            cans: vec![],
            recipes: HashMap::new(),
            recipe_schemas: HashMap::new(),
        }
    }

    #[test]
    fn resolution_includes_matching_instances() {
        let shop = PaintShop {
            cans: vec![
                ("can-1".to_string(), Arc::new(MixedPaint("red")) as Arc<dyn Paint>),
                ("can-2".to_string(), Arc::new(MixedPaint("blue")) as Arc<dyn Paint>),
            ],
            recipes: HashMap::new(),
            recipe_schemas: HashMap::new(),
        };

        let resolution = resolve(&WantsColor("blue"), &shop);
        assert_eq!(resolution.existing_instances, vec!["can-2".to_string()]);
        assert!(resolution.configurable_types.is_empty());
        assert!(!resolution.is_unsatisfiable());
    }

    #[test]
    fn resolution_includes_configurable_types_alongside_instances() {
        let mut shop = empty_shop();
        shop.cans
            .push(("can-1".to_string(), Arc::new(MixedPaint("blue")) as Arc<dyn Paint>));
        shop.recipes.insert(
            "cyan-mix",
            PaintMetadata {
                claimed_colors: vec!["blue", "green"],
            },
        );
        shop.recipes.insert(
            "warm-mix",
            PaintMetadata {
                claimed_colors: vec!["red", "orange"],
            },
        );

        // Both an existing instance AND a configurable type satisfy "blue" --
        // both should be reported, letting the caller offer either path.
        let resolution = resolve(&WantsColor("blue"), &shop);
        assert_eq!(resolution.existing_instances, vec!["can-1".to_string()]);
        assert_eq!(resolution.configurable_types, vec!["cyan-mix"]);
        assert!(!resolution.is_unsatisfiable());
    }

    #[test]
    fn resolution_is_configurable_only_when_no_instance_matches() {
        let mut shop = empty_shop();
        shop.cans
            .push(("can-1".to_string(), Arc::new(MixedPaint("red")) as Arc<dyn Paint>));
        shop.recipes.insert(
            "cyan-mix",
            PaintMetadata {
                claimed_colors: vec!["blue", "green"],
            },
        );

        let resolution = resolve(&WantsColor("blue"), &shop);
        assert!(resolution.existing_instances.is_empty());
        assert_eq!(resolution.configurable_types, vec!["cyan-mix"]);
        assert!(!resolution.is_unsatisfiable());
    }

    #[test]
    fn configurable_type_names_resolve_to_their_config_schema() {
        let mut shop = empty_shop();
        shop.recipes.insert(
            "cyan-mix",
            PaintMetadata {
                claimed_colors: vec!["blue", "green"],
            },
        );
        shop.recipe_schemas
            .insert("cyan-mix", schemars::schema_for!(MixRatio));

        let resolution = resolve(&WantsColor("blue"), &shop);
        assert_eq!(resolution.configurable_types, vec!["cyan-mix"]);

        let schema = shop
            .config_schema(resolution.configurable_types[0])
            .expect("configurable type should have a config schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema with properties");
        assert!(properties.contains_key("parts_base"));
    }

    #[test]
    fn unsatisfiable_when_nothing_could_ever_match() {
        let mut shop = empty_shop();
        shop.recipes.insert(
            "warm-mix",
            PaintMetadata {
                claimed_colors: vec!["red", "orange"],
            },
        );

        let resolution = resolve(&WantsColor("blue"), &shop);
        assert!(resolution.existing_instances.is_empty());
        assert!(resolution.configurable_types.is_empty());
        assert!(resolution.is_unsatisfiable());
    }
}
