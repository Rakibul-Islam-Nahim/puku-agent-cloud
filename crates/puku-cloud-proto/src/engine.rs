//! Which hypervisor a VM runs under.
//!
//! Two engines, side by side, rather than one replacing the other:
//!
//! * `libkrun` -- microsandbox's embedded VMM. What every session ran on
//!   before this type existed, and still the default, so a request or a
//!   worker that has never heard of engines behaves exactly as it did.
//! * `cloud_hypervisor` -- Cloud Hypervisor as a separate process per VM,
//!   Linux/KVM only.
//!
//! A request names the engine it wants; a worker advertises the engines it
//! can run; controld only ever places a VM on a worker that advertised its
//! engine. That last rule is what makes the field safe to add: an older
//! worker ignores `engine` on a spec entirely and would boot libkrun for a
//! request that asked for something else.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum Engine {
    #[default]
    Libkrun,
    CloudHypervisor,
    /// A name this build does not know, kept rather than rejected.
    ///
    /// Failing the whole frame on an unknown engine would drop the
    /// assignment on the floor and leave the session `scheduled` for ever;
    /// carrying it through lets the worker refuse it by name instead.
    Unsupported,
}

impl Engine {
    /// Every engine a build can actually run, in preference order.
    pub const KNOWN: [Engine; 2] = [Engine::Libkrun, Engine::CloudHypervisor];

    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Libkrun => "libkrun",
            Engine::CloudHypervisor => "cloud_hypervisor",
            Engine::Unsupported => "unsupported",
        }
    }

    /// Parse the wire spelling. Unknown names are `None` here so that
    /// configuration parsing can refuse a typo at boot; the serde path maps
    /// them to `Unsupported` instead, for the reason on that variant.
    pub fn parse(s: &str) -> Option<Engine> {
        match s.trim() {
            "libkrun" => Some(Engine::Libkrun),
            "cloud_hypervisor" => Some(Engine::CloudHypervisor),
            _ => None,
        }
    }

    /// Parse a comma-separated list, as operators write it in an env file.
    /// Duplicates collapse; an unknown name is an error naming it.
    pub fn parse_list(raw: &str) -> Result<Vec<Engine>, String> {
        let mut out = Vec::new();
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let e = Engine::parse(part).ok_or_else(|| {
                format!("unknown engine {part:?}; expected libkrun or cloud_hypervisor")
            })?;
            if !out.contains(&e) {
                out.push(e);
            }
        }
        Ok(out)
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Engine {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Engine {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Engine::parse(&s).unwrap_or(Engine::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spelling_round_trips() {
        for e in Engine::KNOWN {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(json, format!("\"{}\"", e.as_str()));
            assert_eq!(serde_json::from_str::<Engine>(&json).unwrap(), e);
        }
    }

    /// A newer controld naming an engine this worker has never heard of
    /// must not fail the frame -- the assignment would silently vanish.
    #[test]
    fn an_unknown_engine_deserializes_as_unsupported() {
        let e: Engine = serde_json::from_str("\"firecracker\"").unwrap();
        assert_eq!(e, Engine::Unsupported);
    }

    #[test]
    fn the_default_is_libkrun() {
        assert_eq!(Engine::default(), Engine::Libkrun);
    }

    #[test]
    fn lists_parse_the_way_operators_write_them() {
        assert_eq!(
            Engine::parse_list("libkrun, cloud_hypervisor,").unwrap(),
            vec![Engine::Libkrun, Engine::CloudHypervisor]
        );
        assert_eq!(Engine::parse_list("libkrun,libkrun").unwrap(), vec![Engine::Libkrun]);
        assert!(Engine::parse_list("").unwrap().is_empty());
        let err = Engine::parse_list("libkrun,kvm").unwrap_err();
        assert!(err.contains("kvm"), "{err}");
    }
}
