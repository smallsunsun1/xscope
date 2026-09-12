//! Project-scoped model routing. Pools are operator-registered, never user URLs.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePool {
    pub id: String,
    pub model: String,
    pub revision: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Stable,
    Canary,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRoute {
    pub name: String,
    pub value: String,
    pub target: RouteTarget,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePolicySpec {
    pub stable_pool: String,
    pub canary_pool: Option<String>,
    #[serde(default)]
    pub canary_percent: u8,
    /// First exact match wins; these headers select cohorts, not permissions.
    #[serde(default)]
    pub headers: Vec<HeaderRoute>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct RoutePolicy {
    pub tenant_id: String,
    pub project_id: String,
    pub model: String,
    pub revision: i64,
    pub spec: RoutePolicySpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PutRoutePolicy {
    /// 0 creates; subsequent writes must match the current revision.
    pub expected_revision: i64,
    pub spec: RoutePolicySpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReleaseAction {
    Pause {
        expected_revision: i64,
    },
    Promote {
        expected_revision: i64,
    },
    Rollback {
        expected_revision: i64,
        target_revision: i64,
    },
}
impl ReleaseAction {
    pub fn expected_revision(&self) -> i64 {
        match self {
            Self::Pause { expected_revision }
            | Self::Promote { expected_revision }
            | Self::Rollback {
                expected_revision, ..
            } => *expected_revision,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pause { .. } => "pause",
            Self::Promote { .. } => "promote",
            Self::Rollback { .. } => "rollback",
        }
    }
}

impl RoutePolicySpec {
    pub fn pause(&self) -> Self {
        let mut next = self.clone();
        next.canary_percent = 0;
        next.headers
            .retain(|rule| rule.target == RouteTarget::Stable);
        next
    }
    pub fn promote(&self) -> Result<Self, String> {
        let mut next = self.clone();
        next.stable_pool = next.canary_pool.take().ok_or("no canary pool to promote")?;
        next.canary_percent = 0;
        // Cohorts belonged to the previous release, not to a future canary.
        next.headers.clear();
        Ok(next)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.stable_pool.trim().is_empty() || self.stable_pool.len() > 128 {
            return Err("stable_pool must be a registered pool ID".into());
        }
        if let Some(canary) = &self.canary_pool
            && (canary.trim().is_empty() || canary.len() > 128 || canary == &self.stable_pool)
        {
            return Err("canary_pool must be nonempty and distinct from stable_pool".into());
        }
        if self.canary_percent > 100 || self.headers.len() > 16 {
            return Err(
                "canary_percent must be 0..100 and at most 16 header rules are allowed".into(),
            );
        }
        if self.canary_pool.is_none()
            && (self.canary_percent > 0
                || self.headers.iter().any(|h| h.target == RouteTarget::Canary))
        {
            return Err("canary traffic requires canary_pool".into());
        }
        for (index, header) in self.headers.iter().enumerate() {
            // Deliberately narrow cohort namespace; never authentication,
            // forwarding, tracing or Envoy destination-control headers.
            if !header.name.starts_with("x-route-")
                || header.name.len() <= 8
                || header.name.len() > 64
                || !header
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || header.value.is_empty()
                || header.value.len() > 256
                || !header.value.bytes().all(|b| (0x21..=0x7e).contains(&b))
            {
                return Err(
                    "rules require lowercase x-route-* names and printable non-space ASCII values"
                        .into(),
                );
            }
            if self.headers[..index]
                .iter()
                .any(|h| h.name == header.name && h.value == header.value)
            {
                return Err("duplicate header match".into());
            }
        }
        Ok(())
    }

    pub fn validate_pools(&self, model: &str, pools: &[RoutePool]) -> Result<(), String> {
        self.validate()?;
        for id in std::iter::once(&self.stable_pool).chain(self.canary_pool.iter()) {
            if !pools
                .iter()
                .any(|pool| &pool.id == id && pool.model == model)
            {
                return Err(format!("pool {id} is not registered for model {model}"));
            }
        }
        Ok(())
    }

    /// `roll` is server-generated entropy in 0..100, never client request ID.
    #[must_use]
    pub fn select(
        &self,
        mut matches: impl FnMut(&HeaderRoute) -> bool,
        roll: u8,
    ) -> (&str, &'static str) {
        if let Some(rule) = self.headers.iter().find(|rule| matches(rule)) {
            return (self.target(rule.target), "header");
        }
        if roll < self.canary_percent {
            (self.target(RouteTarget::Canary), "weight")
        } else {
            (&self.stable_pool, "weight")
        }
    }

    fn target(&self, target: RouteTarget) -> &str {
        match target {
            RouteTarget::Stable => &self.stable_pool,
            RouteTarget::Canary => self.canary_pool.as_deref().unwrap_or(&self.stable_pool),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RoutePolicySpec {
        RoutePolicySpec {
            stable_pool: "stable".into(),
            canary_pool: Some("canary".into()),
            canary_percent: 10,
            headers: vec![],
        }
    }

    #[test]
    fn weights_and_header_precedence() {
        let mut policy = spec();
        assert_eq!(
            (0..100)
                .filter(|roll| policy.select(|_| false, *roll).0 == "canary")
                .count(),
            10
        );
        policy.canary_percent = 0;
        assert_eq!(policy.select(|_| false, 0).0, "stable");
        policy.canary_percent = 100;
        assert_eq!(policy.select(|_| false, 99).0, "canary");
        policy.headers.push(HeaderRoute {
            name: "x-route-cohort".into(),
            value: "stable".into(),
            target: RouteTarget::Stable,
        });
        assert_eq!(policy.select(|_| true, 0), ("stable", "header"));
    }

    #[test]
    fn pause_removes_header_bypass_and_promote_is_a_new_stable() {
        let mut policy = spec();
        policy.headers.push(HeaderRoute {
            name: "x-route-cohort".into(),
            value: "test".into(),
            target: RouteTarget::Canary,
        });
        let paused = policy.pause();
        assert!(paused.validate().is_ok());
        assert!((0..100).all(|roll| paused.select(|_| true, roll).0 == "stable"));
        let promoted = policy.promote().ok();
        assert!(promoted.as_ref().is_some_and(|p| p.stable_pool == "canary"
            && p.canary_pool.is_none()
            && p.headers.is_empty()));
        assert!(promoted.is_some_and(|p| p.promote().is_err()));
    }

    #[test]
    fn reject_invalid_or_unregistered_routes() {
        let mut policy = spec();
        assert!(policy.validate().is_ok());
        assert!(policy.validate_pools("model", &[]).is_err());
        policy.canary_pool = None;
        assert!(policy.validate().is_err());
        policy.canary_percent = 0;
        assert!(policy.validate().is_ok());
        for name in [
            "authorization",
            "x-envoy-cluster",
            "x-gateway-destination-endpoint",
            "x-route-",
            "x-route-\r\n",
        ] {
            policy.headers = vec![HeaderRoute {
                name: name.into(),
                value: "qa".into(),
                target: RouteTarget::Stable,
            }];
            assert!(policy.validate().is_err(), "{name}");
        }
    }
}
