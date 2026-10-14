//! Snapshot sources, helper fallback metadata and scanner generations.

#[cfg(test)]
use anyhow::bail;
use anyhow::{Context, Result};
use dwtop::collector::{Collector, HomeScan, complete_storage};
use dwtop::helper::Client;
use dwtop::i18n::Lang;
use dwtop::model::{HomeErrorKind, HomeRequest, Snapshot};
use dwtop::shutdown::SignalGuard;
use dwtop::{demo, diskstats};
use std::time::Instant;

pub(super) enum Source {
    Live(Box<Collector>),
    /// `outdated` is set once a reply lacked fields that newer helpers send.
    /// Home roots the helper may not scan are scanned by `local_home`
    /// without privileges, as without the helper; `local` is true while it
    /// does.
    Helper {
        client: Client,
        sysfs: diskstats::Sysfs,
        outdated: bool,
        local_home: Box<HomeScan>,
        local: bool,
        generations: HomeGenerations,
    },
    Demo {
        tick: u64,
        start: Instant,
    },
    #[cfg(test)]
    Failing {
        helper: bool,
    },
}

/// Translate each scanner's independent generations into UI acknowledgements.
#[derive(Default)]
pub(super) struct HomeGenerations {
    offset: u64,
}

impl HomeGenerations {
    pub(super) fn switch(&mut self, acknowledged: u64) {
        self.offset = acknowledged;
    }

    pub(super) fn native(&self, acknowledged: u64) -> u64 {
        acknowledged.saturating_sub(self.offset)
    }

    pub(super) fn publish(&self, snapshot: &mut Snapshot) {
        if let Some(tree) = &mut snapshot.home_tree {
            tree.generation = tree.generation.saturating_add(self.offset);
        }
        if let Some(status) = &mut snapshot.home
            && status.generation > 0
        {
            status.generation = status.generation.saturating_add(self.offset);
        }
    }
}

impl Source {
    pub(super) fn sample(
        &mut self,
        request: &HomeRequest,
        shutdown: &SignalGuard,
    ) -> Result<Snapshot> {
        match self {
            Self::Live(collector) => collector.sample(request),
            Self::Helper {
                client,
                sysfs,
                outdated,
                local_home,
                local,
                generations,
            } => {
                let mut helper_request = request.clone();
                helper_request.known_generation = if *local {
                    // A return to the helper must obtain its current tree,
                    // even if the previous local scan had a larger number.
                    0
                } else {
                    generations.native(request.known_generation)
                };
                let mut snapshot = client.sample(&helper_request, || shutdown.requested())?;
                // An older helper sends no fill levels; they need no privilege.
                *outdated |= complete_storage(sysfs, &mut snapshot);
                let refused = snapshot
                    .home
                    .as_ref()
                    .is_some_and(|status| status.error_kind == Some(HomeErrorKind::NotAllowed));
                if refused {
                    if !*local {
                        generations.switch(request.known_generation);
                    }
                    let mut own = request.clone();
                    own.known_generation = generations.native(request.known_generation);
                    let (status, tree) = local_home.sample(&own);
                    snapshot.home = status.map(|mut status| {
                        status.helper_refused = true;
                        status
                    });
                    snapshot.home_tree = tree;
                    *local = true;
                } else if *local {
                    generations.switch(request.known_generation);
                    *local = false;
                    **local_home = HomeScan::unprivileged();
                }
                generations.publish(&mut snapshot);
                Ok(snapshot)
            }
            Self::Demo { tick, start } => {
                let snapshot = demo::snapshot(
                    *tick,
                    start.elapsed().as_secs_f64(),
                    &request.root,
                    request.known_generation,
                );
                *tick += 1;
                Ok(snapshot)
            }
            #[cfg(test)]
            Self::Failing { .. } => bail!("injected sample failure"),
        }
    }

    /// A revoked, outdated, stalled or crashed helper must not stop the
    /// monitor; direct, unprivileged collection replaces it.
    pub(super) fn is_helper(&self) -> bool {
        match self {
            Self::Helper { .. } => true,
            #[cfg(test)]
            Self::Failing { helper } => *helper,
            _ => false,
        }
    }
}

/// Startup notices: English on stderr, the UI language in the terminal.
pub(super) enum Notice {
    Settings(String),
    UnknownKeys(Vec<String>),
    DeviceUnavailable(String),
    HelperOutdated,
}

impl Notice {
    pub(super) fn text(&self, lang: Lang) -> String {
        match self {
            Self::Settings(error) => lang.settings_problem(error),
            Self::UnknownKeys(keys) => lang.unknown_keys(keys),
            Self::DeviceUnavailable(name) => lang.device_unavailable(name),
            Self::HelperOutdated => lang.helper_outdated(),
        }
    }
}

pub(super) fn direct_collector() -> Result<Source> {
    Ok(Source::Live(Box::new(Collector::new(false).context(
        "cannot initialize Linux block device counters",
    )?)))
}
