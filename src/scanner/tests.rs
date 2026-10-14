use super::policy::load_scan_roots_inner;
use super::policy::read_scan_roots;
use super::policy::resolve_trusted_root;
use super::walk::Totals;
use super::walk::Tree;
use super::walk::display_name;
use super::walk::fold;
use super::walk::name_key;
use super::*;
use crate::model::DirNode;
use crate::model::Folded;
use crate::model::HomeReport;
use crate::model::ScanState;
use crate::model::StopReason;
use crate::text::sanitize_bytes;
use std::collections::HashSet;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use std::{
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
};

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "dwtop-scan-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Restore permissions so cleanup can descend everywhere.
        let _ = fs::set_permissions(
            self.0.join("alice/private"),
            fs::Permissions::from_mode(0o755),
        );
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_scan(root: &Path, limits: &Limits) -> HomeReport {
    scan(root, limits, &AtomicBool::new(false), &AtomicU64::new(0)).unwrap()
}

fn wait_report(service: &HomeService, known: u64) -> HomeReport {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(report) = service.report_if_newer(known) {
            return report;
        }
        assert!(Instant::now() < deadline, "scan did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child<'a>(report: &'a HomeReport, parent: u32, name: &str) -> &'a DirNode {
    let node = report.node(parent).unwrap();
    node.children
        .iter()
        .map(|index| report.node(*index).unwrap())
        .find(|node| node.name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn sizes_hardlinks_symlinks_and_unreadable_directories() {
    let tree = TempTree::new("basic");
    let root = &tree.0;
    fs::create_dir_all(root.join("alice/docs/deep")).unwrap();
    fs::create_dir_all(root.join("bob")).unwrap();
    fs::write(root.join("alice/docs/a.bin"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(root.join("alice/docs/deep/b.bin"), vec![2u8; 32 * 1024]).unwrap();
    fs::write(root.join("bob/c.bin"), vec![3u8; 16 * 1024]).unwrap();
    // A hard link to the same inode is counted once.
    fs::hard_link(root.join("bob/c.bin"), root.join("alice/c-link.bin")).unwrap();
    // Symlinks are counted as entries but never followed.
    let outside = TempTree::new("outside");
    fs::write(outside.0.join("huge.bin"), vec![4u8; 256 * 1024]).unwrap();
    symlink(&outside.0, root.join("alice/escape")).unwrap();
    // Non-UTF-8 and control characters are displayed safely.
    fs::create_dir(root.join(std::ffi::OsStr::from_bytes(b"bad\xffname\n"))).unwrap();
    fs::create_dir_all(root.join("alice/private/hidden")).unwrap();
    fs::write(root.join("alice/private/hidden/secret"), vec![5u8; 8192]).unwrap();
    fs::set_permissions(
        root.join("alice/private"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    let report = run_scan(root, &Limits::default());
    let top = report.root_node().unwrap();
    assert_eq!(report.stopped, None);
    let alice = child(&report, 0, "alice");
    let bob = child(&report, 0, "bob");
    let docs = child(&report, report.resolve(&[name_key(b"alice")]).0, "docs");
    assert!(docs.allocated >= 96 * 1024, "{}", docs.allocated);
    assert_eq!(
        docs.apparent,
        96 * 1024
            + fs::metadata(root.join("alice/docs")).unwrap().len()
            + fs::metadata(root.join("alice/docs/deep")).unwrap().len()
    );
    assert_eq!(docs.files, 2);
    assert_eq!(docs.dirs, 1);
    // Exactly one of the two links carries the 16 KiB.
    let linked = alice.own_files.apparent + bob.own_files.apparent;
    assert_eq!(
        linked,
        16 * 1024
            + fs::symlink_metadata(root.join("alice/escape"))
                .unwrap()
                .len()
    );
    assert!(top.allocated < 256 * 1024, "symlink was followed");
    assert!(
        report
            .nodes
            .iter()
            .any(|node| node.name == "bad\\xffname\\x0a"),
        "non-UTF-8 name"
    );
    if !running_as_root() {
        let private = child(&report, report.resolve(&[name_key(b"alice")]).0, "private");
        assert_eq!(private.unreadable, 1);
        assert!(private.partial() && alice.partial() && top.partial());
        assert!(!bob.partial());
    }
    // Display names and stable navigation identities are separate.
    assert_eq!(
        report
            .resolve(&[name_key(b"alice"), name_key(b"docs"), name_key(b"deep")])
            .1,
        3
    );
    assert_eq!(
        report.resolve(&[name_key(b"alice"), name_key(b"gone")]).1,
        1
    );
}

#[test]
fn budgets_depth_and_node_limits_bound_the_result() {
    let tree = TempTree::new("bounds");
    let root = &tree.0;
    for index in 0..30 {
        let directory = root.join(format!("d{index:02}"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("f"), vec![0u8; 4096 * (index + 1)]).unwrap();
    }
    let mut deep = root.join("d29");
    for level in 0..12 {
        deep = deep.join(format!("l{level}"));
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deepest"), vec![0u8; 4096]).unwrap();

    let limits = Limits {
        top_children: 5,
        keep_depth: 3,
        max_depth: 6,
        ..Limits::default()
    };
    let report = run_scan(root, &limits);
    let top = report.root_node().unwrap();
    assert_eq!(top.children.len(), 5);
    // The largest directories are kept, sorted by allocated size.
    assert_eq!(report.node(top.children[0]).unwrap().name, "d29");
    assert_eq!(top.other.dirs, 25);
    let kept: u64 = top
        .children
        .iter()
        .map(|child| report.node(*child).unwrap().allocated)
        .sum();
    assert!(kept + top.other.allocated <= top.allocated);
    assert!(top.truncated, "max_depth must mark the subtree truncated");
    assert!(report.nodes.len() <= 5 + 1 + 3, "{}", report.nodes.len());
    let (_, depth) = report.resolve(&[
        name_key(b"d29"),
        name_key(b"l0"),
        name_key(b"l1"),
        name_key(b"l2"),
    ]);
    assert_eq!(depth, 3, "keep_depth limits retained nodes");

    let report = run_scan(
        root,
        &Limits {
            max_entries: 10,
            ..Limits::default()
        },
    );
    assert_eq!(report.stopped, Some(StopReason::EntryBudget));
    assert_eq!(report.entries, 10);
    let cancelled = scan(
        root,
        &Limits::default(),
        &AtomicBool::new(true),
        &AtomicU64::new(0),
    )
    .unwrap();
    // Cancellation is checked periodically; small trees may finish first.
    assert!(matches!(
        cancelled.stopped,
        None | Some(StopReason::Cancelled)
    ));
    let report = run_scan(
        root,
        &Limits {
            max_nodes: 3,
            ..Limits::default()
        },
    );
    assert!(report.nodes.len() <= 3);
    assert!(report.root_node().unwrap().other.dirs >= 28);
}

#[test]
fn node_budget_prefers_shallow_and_large_directories_over_visit_order() {
    let tree = TempTree::new("budget");
    let root = &tree.0;
    let mut deep = root.join("deep");
    for level in 0..20 {
        deep = deep.join(format!("l{level}"));
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir(deep.with_file_name(format!("side{level}"))).unwrap();
    }
    for (index, size) in [(1, 64), (2, 128), (3, 256)] {
        let directory = root.join(format!("big{index}"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("data"), vec![1u8; size * 1024]).unwrap();
    }
    let report = run_scan(
        root,
        &Limits {
            max_nodes: 6,
            ..Limits::default()
        },
    );
    assert!(report.nodes.len() <= 6, "{}", report.nodes.len());
    let top: Vec<_> = report
        .root_node()
        .unwrap()
        .children
        .iter()
        .map(|index| report.node(*index).unwrap().name.as_str())
        .collect();
    for name in ["big3", "big2", "big1", "deep"] {
        assert!(top.contains(&name), "{name} missing from {top:?}");
    }
    // Everything not retained is still counted in the parents' totals.
    let deep_node = child(&report, 0, "deep");
    assert_eq!(deep_node.dirs, 40);
    let kept: u64 = report.nodes.iter().skip(1).map(|node| node.files).sum();
    assert!(kept <= report.root_node().unwrap().files);
}

#[test]
fn sibling_homes_keep_details_despite_same_depth_flood_in_either_order() {
    // Drive the slab in an explicit DFS order: filesystem readdir order
    // cannot reliably be controlled by creating directories in order.
    let mut results = Vec::new();
    for order in [["alice", "bob", "carol"], ["carol", "bob", "alice"]] {
        let limits = Limits {
            max_nodes: 13,
            keep_depth: 8,
            ..Limits::default()
        };
        let mut tree = Tree::default();
        tree.alloc(DirNode::default(), None, 0);
        let cap = limits.max_nodes * 2 + limits.keep_depth + 1;
        let mut total = 0;
        let mut files = 0;
        for name in order {
            let home = tree.alloc(
                DirNode {
                    name: name.into(),
                    ..DirNode::default()
                },
                Some(0),
                1,
            );
            let count = if name == "alice" { 1000 } else { 3 };
            let mut home_total = 0;
            for number in 1..=count {
                if tree.live >= cap {
                    tree.evict_to(limits.max_nodes + limits.keep_depth);
                }
                let bytes = if name == "alice" {
                    number * 1000
                } else {
                    number
                };
                let child = tree.alloc(
                    DirNode {
                        name: format!("d{number:04}"),
                        allocated: bytes,
                        apparent: bytes,
                        files: 1,
                        own_files: Folded {
                            allocated: bytes,
                            apparent: bytes,
                            files: 1,
                            dirs: 0,
                        },
                        ..DirNode::default()
                    },
                    Some(home),
                    2,
                );
                tree.slots[child].as_mut().unwrap().finished = true;
                tree.node(home).children.push(child as u32);
                home_total += bytes;
                assert!(tree.live <= cap);
                assert!(tree.slots.len() <= cap);
            }
            let slot = tree.slots[home].as_mut().unwrap();
            slot.finished = true;
            slot.node.allocated = home_total;
            slot.node.apparent = home_total;
            slot.node.files = count;
            slot.node.dirs = count;
            tree.node(0).children.push(home as u32);
            total += home_total;
            files += count;
        }
        tree.node(0).allocated = total;
        tree.node(0).files = files;
        tree.evict_to(limits.max_nodes);
        assert_eq!(tree.live, limits.max_nodes);
        let nodes = tree.into_nodes();
        assert_eq!(
            nodes.len(),
            limits.max_nodes,
            "all retained nodes must be reachable"
        );
        assert_eq!(nodes[0].allocated, total);
        assert_eq!(nodes[0].files, files);
        let mut retained = Vec::new();
        for index in &nodes[0].children {
            let home = &nodes[*index as usize];
            assert_eq!(home.children.len(), 3, "{} lost its fair share", home.name);
            let children: Vec<_> = home
                .children
                .iter()
                .map(|index| &nodes[*index as usize])
                .collect();
            assert_eq!(
                home.allocated,
                home.other.allocated + children.iter().map(|node| node.allocated).sum::<u64>()
            );
            assert_eq!(
                home.apparent,
                home.other.apparent + children.iter().map(|node| node.apparent).sum::<u64>()
            );
            assert_eq!(
                home.files,
                home.other.files + children.iter().map(|node| node.files).sum::<u64>()
            );
            assert_eq!(home.dirs, home.other.dirs + children.len() as u64);
            if home.name == "alice" {
                assert_eq!(home.other.dirs, 997, "omitted detail must remain visible");
            } else {
                assert_eq!(home.other.dirs, 0);
            }
            retained.extend(
                children
                    .iter()
                    .map(|node| format!("{}/{}", home.name, node.name)),
            );
        }
        retained.sort();
        results.push(retained);
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn active_deep_descent_cannot_spend_finished_neighbours_shares() {
    let mut tree = Tree::default();
    tree.alloc(DirNode::default(), None, 0);
    for name in ["bob", "carol"] {
        let home = tree.alloc(
            DirNode {
                name: name.into(),
                ..DirNode::default()
            },
            Some(0),
            1,
        );
        let detail = tree.alloc(
            DirNode {
                name: "detail".into(),
                ..DirNode::default()
            },
            Some(home),
            2,
        );
        tree.node(home).children.push(detail as u32);
        tree.node(0).children.push(home as u32);
        tree.slots[detail].as_mut().unwrap().finished = true;
        tree.slots[home].as_mut().unwrap().finished = true;
    }
    let mut ancestors = Vec::new();
    let mut parent = 0;
    for depth in 1..=7 {
        let index = tree.alloc(
            DirNode {
                name: format!("active{depth}"),
                ..DirNode::default()
            },
            Some(parent),
            depth,
        );
        // Active nodes are attached only after finishing, as in Walker.
        ancestors.push(index);
        parent = index;
    }
    for number in 0..50 {
        if tree.live > 7 * 2 + 8 {
            tree.evict_to(7 + 8);
            assert_eq!(tree.slot(1).node.children.len(), 1);
            assert_eq!(tree.slot(3).node.children.len(), 1);
        }
        let index = tree.alloc(
            DirNode {
                name: format!("leaf{number}"),
                ..DirNode::default()
            },
            Some(parent),
            8,
        );
        tree.node(parent).children.push(index as u32);
        tree.slots[index].as_mut().unwrap().finished = true;
    }
    for index in ancestors.into_iter().rev() {
        let parent = tree.slot(index).parent.unwrap();
        tree.slots[index].as_mut().unwrap().finished = true;
        tree.node(parent).children.push(index as u32);
    }
    tree.evict_to(7);
    assert_eq!(tree.live, 7);
    let nodes = tree.into_nodes();
    assert_eq!(nodes.len(), 7);
    for home in nodes[0]
        .children
        .iter()
        .map(|index| &nodes[*index as usize])
    {
        assert_eq!(home.children.len(), 1, "{} lost its detail", home.name);
    }
}

#[test]
fn fair_eviction_preserves_ancestors_and_full_scan_totals() {
    let tree = TempTree::new("fair-budget");
    for (user, count, size) in [("alice", 80, 8192), ("bob", 3, 1), ("carol", 3, 1)] {
        for index in 0..count {
            let directory = tree.0.join(format!("{user}/d{index:03}/nested/deep"));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("data"), vec![1u8; size]).unwrap();
        }
    }
    let full = run_scan(&tree.0, &Limits::default());
    for top_children in [3, 200] {
        for max_nodes in [1, 3, 16] {
            let bounded = run_scan(
                &tree.0,
                &Limits {
                    max_nodes,
                    top_children,
                    ..Limits::default()
                },
            );
            assert_eq!(bounded.stopped, None);
            assert_eq!(bounded.nodes.len(), max_nodes);
            if max_nodes == 16 {
                for user in ["alice", "bob", "carol"] {
                    assert!(
                        !child(&bounded, 0, user).children.is_empty(),
                        "{user} lost all details"
                    );
                }
            }
            let mut queue = vec![(0, 0)];
            let mut seen = HashSet::new();
            while let Some((small_index, full_index)) = queue.pop() {
                assert!(seen.insert(small_index));
                let small = bounded.node(small_index).unwrap();
                let original = full.node(full_index).unwrap();
                assert_eq!(small.allocated, original.allocated);
                assert_eq!(small.apparent, original.apparent);
                assert_eq!(small.files, original.files);
                assert_eq!(small.dirs, original.dirs);
                assert_eq!(small.truncated, original.truncated);
                let mut omitted = Folded::default();
                for original_child in &original.children {
                    let original_node = full.node(*original_child).unwrap();
                    match small
                        .children
                        .iter()
                        .find(|index| bounded.node(**index).unwrap().name == original_node.name)
                    {
                        Some(index) => queue.push((*index, *original_child)),
                        None => fold(&mut omitted, &Totals::of(original_node)),
                    }
                }
                assert_eq!(small.other.allocated, omitted.allocated);
                assert_eq!(small.other.apparent, omitted.apparent);
                assert_eq!(small.other.files, omitted.files);
                assert_eq!(small.other.dirs, omitted.dirs);
            }
            assert_eq!(seen.len(), bounded.nodes.len(), "ancestor closure");
            assert!(bounded.nodes.iter().any(|node| node.other.dirs > 0));
        }
    }
}

#[test]
fn hardlink_limit_is_reported_and_other_filesystems_are_skipped() {
    let tree = TempTree::new("links");
    let root = &tree.0;
    for index in 0..4 {
        fs::write(root.join(format!("f{index}")), vec![0u8; 4096]).unwrap();
        fs::hard_link(
            root.join(format!("f{index}")),
            root.join(format!("g{index}")),
        )
        .unwrap();
    }
    let report = run_scan(
        root,
        &Limits {
            max_hardlinks: 2,
            ..Limits::default()
        },
    );
    assert!(report.hardlink_limit_reached);
    // /dev usually contains separate mounts (pts, shm, mqueue).
    let dev = run_scan(Path::new("/dev"), &Limits::default());
    let pts_is_mount = fs::metadata("/dev/pts").is_ok_and(|pts| {
        use std::os::unix::fs::MetadataExt;
        fs::metadata("/dev").is_ok_and(|dev| dev.dev() != pts.dev())
    });
    if pts_is_mount {
        assert!(dev.other_filesystems >= 1);
    }
    assert!(
        scan(
            Path::new("/nonexistent-dwtop"),
            &Limits::default(),
            &AtomicBool::new(false),
            &AtomicU64::new(0)
        )
        .is_err()
    );
}

#[test]
fn helper_policy_allows_only_listed_roots_and_never_follows_symlinks_below() {
    let tree = TempTree::new("policy");
    let allowed = tree.0.join("home");
    fs::create_dir_all(allowed.join("alice/docs")).unwrap();
    let outside = TempTree::new("policy-outside");
    // Large enough to stand out from directory blocks on any filesystem.
    fs::write(outside.0.join("secret.bin"), vec![1u8; 1 << 20]).unwrap();
    symlink(&outside.0, allowed.join("alice/link")).unwrap();
    // A symlinked allowed root (/home -> /var/home) is resolved once.
    symlink(&allowed, tree.0.join("home-link")).unwrap();
    let policy = RootPolicy::Allowed(vec![AllowedRoot {
        configured: tree.0.join("home-link"),
        canonical: fs::canonicalize(&allowed).unwrap(),
    }]);
    for path in [
        tree.0.join("home-link"),
        tree.0.join("home-link/alice"),
        allowed.join("alice/docs"),
    ] {
        assert!(policy.allows(&path), "{}", path.display());
        assert!(policy.open(&path).is_ok(), "{}", path.display());
    }
    let report = scan_with(
        &tree.0.join("home-link/alice"),
        &policy,
        &Limits::default(),
        &AtomicBool::new(false),
        &AtomicU64::new(0),
    )
    .unwrap();
    assert!(report.root_node().unwrap().allocated < 1 << 19);
    // Outside the list, and a symlink below an allowed root.
    assert!(!policy.allows(&outside.0));
    assert!(!policy.allows(&tree.0.join("home2")));
    let error = policy.open(&outside.0).unwrap_err();
    assert!(is_not_allowed(&error), "{error}");
    let escape = allowed.join("alice/link");
    assert!(policy.allows(&escape), "lexically below the root");
    let error = policy.open(&escape).unwrap_err();
    assert!(is_not_allowed(&error), "{error}");
    let error = policy.open(&allowed.join("alice/missing")).unwrap_err();
    assert!(!is_not_allowed(&error) && error.kind() == io::ErrorKind::NotFound);
    // Without the helper, the root itself may be a symlink, like du.
    assert!(RootPolicy::Follow.open(&escape).is_ok());
}

#[test]
fn scan_roots_default_to_home_and_unsafe_files_allow_nothing() {
    let tree = TempTree::new("roots");
    // / is trusted even when tests run as an ordinary user. /tmp and the
    // user-owned fixture are deliberately not trusted policy ancestors.
    let missing = PathBuf::from(format!(
        "/nonexistent-dwtop-policy-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    assert!(!missing.exists());
    let (policy, problem) = load_scan_roots(&missing.join("scan-roots"));
    assert!(problem.is_none());
    let RootPolicy::Allowed(roots) = &policy else {
        panic!("helper policy expected");
    };
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].configured, Path::new("/home"));
    assert!(policy.allows(Path::new("/home/alice")));
    assert!(!policy.allows(Path::new("/root")) && !policy.allows(Path::new("/")));
    // A file the test user owns is not trusted: nothing may be scanned.
    let file = tree.0.join("scan-roots");
    fs::write(&file, "/srv\n").unwrap();
    let (policy, problem) = load_scan_roots(&file);
    if !running_as_root() {
        assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
        assert!(problem.unwrap().contains("root-owned"));
        assert!(!policy.allows(Path::new("/srv")) && !policy.allows(Path::new("/home")));
    }
}

#[test]
fn user_replaceable_scan_roots_never_authorize_startup_aliases() {
    let tree = TempTree::new("startup-alias");
    let root = tree.0.join("projects");
    fs::create_dir(&root).unwrap();
    let configured = root.clone();
    let canonical = resolve_trusted_root(&configured).unwrap();
    let initial = RootPolicy::Allowed(vec![AllowedRoot {
        configured: configured.clone(),
        canonical,
    }]);
    assert!(initial.open(&root).is_ok());
    fs::remove_dir(&root).unwrap();
    symlink("/", &root).unwrap();
    assert!(
        initial.open(&root).is_err(),
        "replacement after startup must fail"
    );
    let canonical = resolve_trusted_root(&configured).unwrap_or_else(|_| configured.clone());
    let restarted = RootPolicy::Allowed(vec![AllowedRoot {
        configured,
        canonical,
    }]);
    assert!(
        !restarted.allows(Path::new("/")),
        "replacement before startup must not grant /"
    );
    assert!(restarted.open(&root).is_err());
    assert!(
        resolve_trusted_root(Path::new("/proc/self")).is_err(),
        "magic aliases are not administrator-owned"
    );
}

#[test]
fn policy_migration_never_uses_defaults_below_unsafe_ancestors() {
    let tree = TempTree::new("policy-migration");
    let missing = PathBuf::from(format!(
        "/nonexistent-dwtop-migration-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let current = missing.join("dwtop/scan-roots");
    let legacy = missing.join("disktop/scan-roots");
    let (policy, problem) = load_scan_roots_with_legacy(&current, &legacy, &legacy);
    assert!(problem.is_none());
    assert!(policy.allows(Path::new("/home/alice")));
    assert!(!policy.allows(Path::new("/root")));

    // Missing policy below a writable/user-owned parent is an error,
    // whether it is the current policy or the legacy fallback.
    let unsafe_missing = tree.0.join("missing/scan-roots");
    for (new, old) in [(&unsafe_missing, &legacy), (&current, &unsafe_missing)] {
        let (policy, problem) = load_scan_roots_with_legacy(new, old, old);
        assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
        assert!(problem.unwrap().contains("root-owned"));
    }

    // /proc/self is a root-owned symlink under trusted /proc. Looking only
    // at the missing leaf would miss it and incorrectly enable /home.
    let symlink_parent = Path::new("/proc/self/nonexistent-dwtop-policy/scan-roots");
    for (new, old) in [
        (symlink_parent, legacy.as_path()),
        (current.as_path(), symlink_parent),
    ] {
        let (policy, problem) = load_scan_roots_with_legacy(new, old, old);
        assert_eq!(policy, RootPolicy::Allowed(Vec::new()));
        assert!(problem.unwrap().contains("/proc/self"));
    }
}

#[test]
fn long_names_are_capped_and_aborted_scans_never_set_growth() {
    let long = vec![0xffu8; 255];
    let name = display_name(&long);
    assert_eq!(name.chars().count(), MAX_NAME_CHARS);
    assert!(name.ends_with('~'));
    assert_eq!(display_name(b"short"), "short");

    let tree = TempTree::new("aborted");
    for index in 0..40 {
        fs::create_dir(tree.0.join(format!("d{index}"))).unwrap();
    }
    let wait = |service: &HomeService, known: u64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(report) = service.report_if_newer(known) {
                return report;
            }
            assert!(Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // The first scan hits the entry budget: it is no growth baseline.
    let service = HomeService::start(
        tree.0.clone(),
        Duration::from_secs(3600),
        Limits {
            max_entries: 10,
            ..Limits::default()
        },
        false,
        RootPolicy::Follow,
        || Ok(()),
    )
    .unwrap();
    let first = wait(&service, 0);
    assert_eq!(first.stopped, Some(StopReason::EntryBudget));
    assert!(first.nodes.iter().all(|node| node.growth_start.is_none()));
    service.rescan();
    let second = wait(&service, 1);
    assert!(
        second
            .nodes
            .iter()
            .all(|node| node.growth_previous.is_none())
    );
    drop(service);
    // After a complete scan, an aborted one is not annotated either.
    let service = HomeService::start(
        tree.0.clone(),
        Duration::from_secs(3600),
        Limits::default(),
        false,
        RootPolicy::Follow,
        || Ok(()),
    )
    .unwrap();
    let complete = wait(&service, 0);
    assert_eq!(complete.stopped, None);
    service.rescan();
    let again = wait(&service, 1);
    assert_eq!(again.root_node().unwrap().growth_previous, Some(0));
}

#[test]
fn policy_limit_rejects_partial_paths_and_accepts_exact_bound() {
    let tree = TempTree::new("policy-limit");
    // Both current and legacy policies use this reader. A long comment
    // puts the limit exactly after the slash of the otherwise narrow root.
    for filename in ["scan-roots", "legacy-scan-roots"] {
        let path = tree.0.join(filename);
        let dangerous = format!("#{}\n/srv/private\n", "x".repeat(MAX_POLICY_BYTES - 3));
        assert_eq!(&dangerous.as_bytes()[MAX_POLICY_BYTES - 1..][..1], b"/");
        fs::write(&path, dangerous).unwrap();
        let error = read_scan_roots(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no helper scans are allowed"));

        let accepted = format!("#{}\n/home", "x".repeat(MAX_POLICY_BYTES - 7));
        assert_eq!(accepted.len(), MAX_POLICY_BYTES);
        fs::write(&path, accepted).unwrap();
        assert_eq!(read_scan_roots(&path).unwrap(), ["/home"]);
    }
}

#[test]
fn malformed_policy_rejects_the_entire_allowlist() {
    let tree = TempTree::new("invalid-policy");
    let path = tree.0.join("scan-roots");
    for contents in [
        b"/home\nrelative\n".to_vec(),
        b"/home\n/srv/../etc\n".to_vec(),
        b"/home\n\xff\n".to_vec(),
        "/home\n".repeat(MAX_SCAN_ROOTS + 1).into_bytes(),
    ] {
        fs::write(&path, contents).unwrap();
        assert_eq!(
            read_scan_roots(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    fs::write(&path, "# no privileged roots\n").unwrap();
    assert!(read_scan_roots(&path).unwrap().is_empty());
}

#[test]
fn disappearing_selected_policy_never_enables_defaults() {
    let path = PathBuf::from(format!(
        "/dwtop-policy-missing-{}/scan-roots",
        std::process::id()
    ));
    let (policy, error) = load_scan_roots_inner(&path, false);
    assert!(!policy.allows(Path::new("/home")));
    assert!(error.unwrap().contains("disappeared"));
}

#[test]
fn colliding_display_names_keep_navigation_and_growth_distinct() {
    let tree = TempTree::new("name-identity");
    let mut names = Vec::new();
    for prefix in [vec![0xff; 64], b"\\xff".repeat(60)] {
        for suffix in *b"ab" {
            let mut name = prefix.clone();
            name.push(suffix);
            let path = tree.0.join(std::ffi::OsString::from_vec(name.clone()));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join(format!("child-{}", char::from(suffix)))).unwrap();
            fs::write(
                path.join("data"),
                vec![0u8; if suffix == b'a' { 8192 } else { 65536 }],
            )
            .unwrap();
            names.push(name);
        }
    }
    for pair in names.as_chunks::<2>().0 {
        assert_eq!(display_name(&pair[0]), display_name(&pair[1]));
        assert_ne!(name_key(&pair[0]), name_key(&pair[1]));
    }
    assert_eq!(name_key(&vec![0xff; 255]).len(), 510);
    let service = HomeService::start(
        tree.0.clone(),
        Duration::from_secs(3600),
        Limits::default(),
        false,
        RootPolicy::Follow,
        || Ok(()),
    )
    .unwrap();
    let first = wait_report(&service, 0);
    for name in &names {
        let key = name_key(name);
        let (index, depth) = first.resolve(std::slice::from_ref(&key));
        assert_eq!(depth, 1);
        let node = first.node(index).unwrap();
        assert_eq!(node.identity(), key);
        assert_eq!(node.name, display_name(name));
        let leaf = format!("child-{}", char::from(*name.last().unwrap()));
        let (leaf_index, depth) = first.resolve(&[key, name_key(leaf.as_bytes())]);
        assert_eq!(depth, 2);
        assert_eq!(first.node(leaf_index).unwrap().name, leaf);
    }
    service.rescan();
    let second = wait_report(&service, first.generation);
    for node in &second.nodes {
        assert_eq!(node.growth_previous, Some(0), "{}", node.identity());
        assert_eq!(node.growth_start, Some(0));
    }
}

#[test]
fn unreadable_scan_does_not_replace_complete_growth_baseline() {
    if running_as_root() {
        return; // Root bypasses the permissions needed for this fixture.
    }
    let tree = TempTree::new("partial-growth");
    let private = tree.0.join("alice/private");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("data"), vec![0u8; 128 * 1024]).unwrap();
    let service = HomeService::start(
        tree.0.clone(),
        Duration::from_secs(3600),
        Limits::default(),
        false,
        RootPolicy::Follow,
        || Ok(()),
    )
    .unwrap();
    let first = wait_report(&service, 0);
    assert!(!first.root_node().unwrap().partial());
    fs::set_permissions(&private, fs::Permissions::from_mode(0o0)).unwrap();
    service.rescan();
    let partial = wait_report(&service, first.generation);
    assert_eq!(partial.stopped, None);
    assert!(partial.root_node().unwrap().partial());
    assert!(
        partial
            .nodes
            .iter()
            .all(|node| node.growth_previous.is_none() && node.growth_start.is_none())
    );
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    service.rescan();
    let recovered = wait_report(&service, partial.generation);
    assert!(!recovered.root_node().unwrap().partial());
    assert_eq!(
        recovered.root_node().unwrap().allocated,
        first.root_node().unwrap().allocated
    );
    assert_eq!(recovered.root_node().unwrap().growth_previous, Some(0));
    assert_eq!(recovered.root_node().unwrap().growth_start, Some(0));
}

#[test]
fn depth_and_hardlink_limits_exclude_growth_but_retention_folding_does_not() {
    let tree = TempTree::new("growth-completeness");
    fs::create_dir_all(tree.0.join("deep/nested")).unwrap();
    fs::write(tree.0.join("deep/nested/data"), vec![0u8; 8192]).unwrap();
    fs::hard_link(tree.0.join("deep/nested/data"), tree.0.join("linked")).unwrap();
    for (limits, incomplete) in [
        (
            Limits {
                max_depth: 1,
                ..Limits::default()
            },
            true,
        ),
        (
            Limits {
                max_hardlinks: 0,
                ..Limits::default()
            },
            true,
        ),
        (
            Limits {
                max_nodes: 1,
                ..Limits::default()
            },
            false,
        ),
    ] {
        let service = HomeService::start(
            tree.0.clone(),
            Duration::from_secs(3600),
            limits,
            false,
            RootPolicy::Follow,
            || Ok(()),
        )
        .unwrap();
        let first = wait_report(&service, 0);
        assert_eq!(first.stopped, None);
        assert_eq!(
            first.root_node().unwrap().partial() || first.hardlink_limit_reached,
            incomplete
        );
        service.rescan();
        let second = wait_report(&service, first.generation);
        if incomplete {
            assert!(
                second
                    .nodes
                    .iter()
                    .all(|node| node.growth_previous.is_none() && node.growth_start.is_none())
            );
        } else {
            assert!(second.root_node().unwrap().other.dirs > 0);
            assert_eq!(second.root_node().unwrap().growth_previous, Some(0));
            assert_eq!(second.root_node().unwrap().growth_start, Some(0));
        }
    }
}

#[test]
fn root_validation() {
    for valid in ["/", "/home", "/srv/home dirs", "/home/Grüße"] {
        assert!(validate_root(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "home",
        "/home/",
        "//home",
        "/home/../etc",
        "/home/./x",
        "/a\nb",
    ] {
        assert!(validate_root(invalid).is_err(), "{invalid:?}");
    }
    assert!(validate_root(&format!("/{}", "a".repeat(MAX_ROOT_BYTES))).is_err());
}

#[test]
fn service_scans_reports_growth_and_follows_reconfiguration() {
    let tree = TempTree::new("service");
    fs::create_dir(tree.0.join("grow")).unwrap();
    let service = HomeService::start(
        tree.0.clone(),
        Duration::from_secs(3600),
        Limits::default(),
        false,
        RootPolicy::Follow,
        || Ok(()),
    )
    .unwrap();
    let wait = |known: u64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(report) = service.report_if_newer(known) {
                return report;
            }
            assert!(Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let first = wait(0);
    assert_eq!(first.generation, 1);
    assert_eq!(first.root_node().unwrap().growth_previous, None);
    assert_eq!(service.status().state, ScanState::Done);
    assert!(service.report_if_newer(1).is_none());
    fs::write(tree.0.join("grow/new.bin"), vec![7u8; 128 * 1024]).unwrap();
    service.rescan();
    let second = wait(1);
    let grow = child(&second, 0, "grow");
    assert!(grow.growth_previous.unwrap() >= 128 * 1024);
    assert_eq!(grow.growth_previous, grow.growth_start);
    let other = TempTree::new("service-other");
    service.configure(other.0.clone(), Duration::from_secs(3600));
    let third = wait(2);
    assert_eq!(third.root, sanitize_bytes(other.0.as_os_str().as_bytes()));
    assert_eq!(
        third.root_node().unwrap().growth_previous,
        None,
        "new root resets growth"
    );
    service.configure(
        PathBuf::from("/nonexistent-dwtop"),
        Duration::from_secs(3600),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while service.status().state != ScanState::Failed {
        assert!(
            Instant::now() < deadline,
            "missing root did not fail visibly"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        service
            .status()
            .error
            .unwrap()
            .contains("nonexistent-dwtop")
    );
}
