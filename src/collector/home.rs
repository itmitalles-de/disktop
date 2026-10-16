//! Request validation and scheduling for the optional home scanner.

use crate::{
    model::{HomeErrorKind, HomeReport, HomeRequest, HomeStatus, ScanState},
    privilege,
    scanner::{self, HomeService, Limits, RootPolicy},
    text::sanitize,
};
use std::{path::PathBuf, time::Duration};

/// The home scanner as the collector drives it from requests: validation,
/// the helper's root policy, starting and reconfiguring the service.
pub struct HomeScan {
    service: Option<HomeService>,
    limits: Limits,
    privileged: bool,
    policy: RootPolicy,
    /// Why the helper's scan-roots file was rejected, if it was.
    policy_problem: Option<String>,
}

impl HomeScan {
    pub fn new(
        limits: Limits,
        privileged: bool,
        policy: RootPolicy,
        policy_problem: Option<String>,
    ) -> Self {
        Self {
            service: None,
            limits,
            privileged,
            policy,
            policy_problem,
        }
    }

    /// The UI's own scanner, used without the helper or for roots the helper
    /// may not scan.
    pub fn unprivileged() -> Self {
        Self::new(Limits::default(), false, RootPolicy::Follow, None)
    }

    pub fn sample(&mut self, request: &HomeRequest) -> (Option<HomeStatus>, Option<HomeReport>) {
        if request.root.is_empty() {
            return (None, None);
        }
        let privileged = self.privileged;
        let failed = |kind: HomeErrorKind, error: String| {
            (
                Some(HomeStatus {
                    root: sanitize(&request.root),
                    state: ScanState::Failed,
                    error: Some(sanitize(&error)),
                    error_kind: Some(kind),
                    privileged,
                    ..HomeStatus::default()
                }),
                None,
            )
        };
        if let Err(error) = scanner::validate_root(&request.root) {
            return failed(error.0, error.to_string());
        }
        let root = PathBuf::from(&request.root);
        if !self.policy.allows(&root) {
            // Stop scanning the previous root with privileges.
            self.service = None;
            let problem = self
                .policy_problem
                .as_ref()
                .map_or_else(String::new, |problem| format!(" ({problem})"));
            return failed(
                HomeErrorKind::NotAllowed,
                format!(
                    "{} is not below a helper scan root in {}{problem}",
                    request.root,
                    scanner::SCAN_ROOTS_PATH
                ),
            );
        }
        let interval = Duration::from_secs(u64::from(request.interval_minutes.clamp(1, 1440)) * 60);
        if request.scan && self.service.is_none() {
            match HomeService::start(
                root.clone(),
                interval,
                self.limits.clone(),
                self.privileged,
                self.policy.clone(),
                privilege::prepare_scan_thread,
            ) {
                Ok(service) => self.service = Some(service),
                Err(error) => {
                    return failed(
                        HomeErrorKind::StartFailed,
                        format!("cannot start scanner: {error}"),
                    );
                }
            }
        }
        let Some(service) = &self.service else {
            return (
                Some(HomeStatus {
                    root: sanitize(&request.root),
                    privileged: self.privileged,
                    ..HomeStatus::default()
                }),
                None,
            );
        };
        service.configure(root, interval);
        if request.rescan {
            service.rescan();
        }
        (
            Some(service.status()),
            service.report_if_newer(request.known_generation),
        )
    }
}
