//! Platforms, the five universal axes, and the [`Configuration`] keying unit (§6).

use std::fmt;

/// A target platform: a name plus a target triple (§6.1). Constraints beyond the
/// triple are deferred; the triple is the load-bearing field in Milestone 1.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Platform {
    name: String,
    target_triple: String,
}

impl Platform {
    pub fn new(name: impl Into<String>, target_triple: impl Into<String>) -> Self {
        Platform {
            name: name.into(),
            target_triple: target_triple.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn target_triple(&self) -> &str {
        &self.target_triple
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.target_triple)
    }
}

/// The five universal axes (§6.2). Each rule declares which it consumes; axes a
/// rule does not consume are excluded from its cache key ([`AxisValues::consumed`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Axis {
    OptLevel,
    Lto,
    DebugInfo,
    Sanitizer,
    Coverage,
}

impl Axis {
    /// Stable name used in cache keys and `--flag` mapping.
    pub fn name(&self) -> &'static str {
        match self {
            Axis::OptLevel => "opt_level",
            Axis::Lto => "lto",
            Axis::DebugInfo => "debug_info",
            Axis::Sanitizer => "sanitizer",
            Axis::Coverage => "coverage",
        }
    }
}

/// Canonical axis ordering. [`AxisValues::consumed`] iterates this so a cache key is
/// independent of the order a rule happens to declare its consumed axes in.
pub const ALL_AXES: [Axis; 5] = [
    Axis::OptLevel,
    Axis::Lto,
    Axis::DebugInfo,
    Axis::Sanitizer,
    Axis::Coverage,
];

macro_rules! axis_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? } default $default:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// Stable string form for cache keys and flag mapping.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $s),+
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                $name::$default
            }
        }
    };
}

axis_enum!(
    /// Optimization level (§6.2). Host default: `debug`.
    OptLevel {
        Debug => "debug",
        Release => "release",
        ReleaseWithDebugInfo => "release_with_debuginfo",
    }
    default Debug
);

axis_enum!(
    /// Link-time optimization (§6.2). Host default: `off`.
    Lto {
        Off => "off",
        Thin => "thin",
        Full => "full",
    }
    default Off
);

axis_enum!(
    /// Debug info level (§6.2). Host default: `full` (matches a debug build).
    DebugInfo {
        None => "none",
        LineTablesOnly => "line_tables_only",
        Full => "full",
    }
    default Full
);

axis_enum!(
    /// Sanitizer (§6.2). Host default: `none`.
    Sanitizer {
        None => "none",
        Address => "address",
        Thread => "thread",
        Memory => "memory",
        Undefined => "undefined",
    }
    default None
);

axis_enum!(
    /// Coverage instrumentation (§6.2). Host default: `off`.
    Coverage {
        On => "on",
        Off => "off",
    }
    default Off
);

/// The values of all five axes. A pure record with no cross-field invariant, so its
/// fields are public; [`Default`] gives the host defaults (§6.6).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct AxisValues {
    pub opt_level: OptLevel,
    pub lto: Lto,
    pub debug_info: DebugInfo,
    pub sanitizer: Sanitizer,
    pub coverage: Coverage,
}

impl AxisValues {
    /// The stable string value of a single axis.
    pub fn value_str(&self, axis: Axis) -> &'static str {
        match axis {
            Axis::OptLevel => self.opt_level.as_str(),
            Axis::Lto => self.lto.as_str(),
            Axis::DebugInfo => self.debug_info.as_str(),
            Axis::Sanitizer => self.sanitizer.as_str(),
            Axis::Coverage => self.coverage.as_str(),
        }
    }

    /// Project onto only the `consumed` axes, in canonical order, as
    /// `(axis_name, value)` pairs — the **cache-key trimming** of §6.2.
    ///
    /// This crate provides the canonical *data*; the actual hashing lives in
    /// `anneal-exec` (the deep module that owns cache keys). A rule consuming no
    /// axes (e.g. `nickel_eval`) yields an empty vector, making its output shareable
    /// across all configurations.
    pub fn consumed(&self, consumed: &[Axis]) -> Vec<(&'static str, &'static str)> {
        ALL_AXES
            .iter()
            .filter(|a| consumed.contains(a))
            .map(|a| (a.name(), self.value_str(*a)))
            .collect()
    }
}

/// A configured target's configuration: `(Platform, AxisValues)` (§3.3). This is the
/// unit that, combined with a [`crate::Label`], identifies a configured target.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Configuration {
    platform: Platform,
    axes: AxisValues,
}

impl Configuration {
    pub fn new(platform: Platform, axes: AxisValues) -> Self {
        Configuration { platform, axes }
    }

    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    pub fn axes(&self) -> &AxisValues {
        &self.axes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_defaults() {
        let a = AxisValues::default();
        assert_eq!(a.opt_level, OptLevel::Debug);
        assert_eq!(a.lto, Lto::Off);
        assert_eq!(a.sanitizer, Sanitizer::None);
        assert_eq!(a.coverage, Coverage::Off);
    }

    #[test]
    fn trimming_keeps_only_consumed_in_canonical_order() {
        let a = AxisValues {
            opt_level: OptLevel::Release,
            coverage: Coverage::On,
            ..Default::default()
        };
        // Declared out of order; result must be canonical (opt_level before coverage).
        let consumed = a.consumed(&[Axis::Coverage, Axis::OptLevel]);
        assert_eq!(
            consumed,
            vec![("opt_level", "release"), ("coverage", "on")]
        );
    }

    #[test]
    fn no_consumed_axes_is_configuration_invariant() {
        // A rule like `nickel_eval` consumes nothing -> empty key contribution.
        assert!(AxisValues::default().consumed(&[]).is_empty());
        let a = AxisValues {
            opt_level: OptLevel::Release,
            ..Default::default()
        };
        let b = AxisValues::default();
        assert_eq!(a.consumed(&[]), b.consumed(&[]));
    }
}
