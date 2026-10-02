//! What a path's containment means, in one place.
//!
//! # Why this module exists
//!
//! Peek asks one question in several places — *is this path inside this repository?* — and the
//! answer has to be the same one each time, because the callers disagree about what a path is. A
//! path can name a file that has already been deleted, can be spelled through a symlink that
//! leaves the tree, and can spell a climb out of the root as a `..` that no amount of asking the
//! filesystem will settle. Three sites in this crate answered it three different ways: one resolved
//! the whole path and degraded to nothing when that failed, one handed back the absolute path
//! unchanged, and one compared the spelling without reading what the spelling said.
//!
//! `crates/peek-cli/docs/path-trust.md` works the four possible notions out in full. Two of them
//! answer the question and are implemented here:
//!
//! - [`relative_path`] is **lexical containment**: the spelling, normalised, compared against the
//!   root component by component. It touches no filesystem, so it has an answer for a path that has
//!   never existed, and it is the right rule where the question is *what name does this path have
//!   inside the repository*.
//! - [`contains`] is **trusted-ancestor containment**: [`relative_path`] applied to the location
//!   each side reaches, which is its deepest existing prefix resolved with the names that follow it
//!   put back on the end ([`resolve_location`]). It is the right rule where the question is *where
//!   is this thing*, because a directory reached under a second name is one directory and only the
//!   filesystem can say so.
//!
//! The second is not a rival to the first. It is the first rule applied to located spellings, and
//! it is written that way so the crate has one comparison walk rather than one per caller: a caller
//! cannot accept a path at a gate and then fail to strip the base from it, because there is nothing
//! left to disagree with.
//!
//! # Why the callers do not all use the same one
//!
//! | Caller | Rule | Why that one there |
//! |---|---|
//! | [`crate::indexer::refresh`] | lexical | The key it derives has to be the key the discovery walk derived. Discovery names a file by the path it was walked under and does not resolve a symlink before doing so, so resolving here would key a followed link by its target and make an incremental build disagree with a full one about the same repository. |
//! | [`crate::watch::plan_batch`] | lexical | It is a pure function over paths an operating system reported, and it has to stay one: it runs on every batch, its whole suite runs without a filesystem, and a path that reads as outside is counted rather than looked up. |
//! | [`crate::doctor`]'s index-location check | located | Both arguments are independent filesystem locations — the file a store was opened at, and a root somebody else resolved — so they can be two spellings of one directory. The question is about a place, not about a name. |
//! | the CLI's `relative_to` | both, in that order | A user may name a path that does not exist, so the gate has to be answerable without a filesystem, and a link out of the tree has to be caught, so the answer is the located one. `docs/path-trust.md` gives the order and the reasons. |
//!
//! **Neither rule is the other one weakened.** The lexical rule refuses a climb out of the root
//! because the arithmetic says so; it does not refuse a directory link pointing out of the tree,
//! because no component of that spelling names a location and only the filesystem can. The located
//! rule catches both, at the cost of having nothing to say about a path nothing can open — which is
//! the whole reason the two callers above that use the lexical rule do: they are asked for a *name*
//! to key an index row under, and a name is what the full build would have used for the same file.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf, Prefix};

/// `path` expressed relative to `root`, or `None` when the spelling is not inside it.
///
/// **The lexical rule, and the one that is available everywhere.** `.`, `//` and `..` are resolved
/// by this function, in this function, against `root`; the filesystem is not consulted, so there is
/// an answer for a path that has never existed, for a volume that is not mounted, and for a share
/// that is not answering.
///
/// The comparison is component-wise and never a string prefix, so `<parent>/repo-old` is not inside
/// `<parent>/repo` — which is the mistake that would put one repository's index inside another's
/// tree, or strip one repository's rows with a command aimed at another.
///
/// **What it is deliberately not.** It does not follow a symlink, so `<root>/link/secret.rs` where
/// `link` points out of the tree is *inside* here. That is not a hole in the rule; it is the
/// question being different. Where the answer must be a location rather than a name, use
/// [`contains`].
pub fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    strip_base(&normalise_lexically(root), &normalise_lexically(path))
}

/// Whether `root` contains `path`.
///
/// **The located rule**, which is [`relative_path`] applied to the location each side reaches
/// rather than to the spelling. A path that has never existed is still answerable, because the walk
/// resolves the deepest component that does exist and the components that follow it are names under
/// a directory whose location has been verified.
///
/// That is what catches a directory link inside the repository that points out of it: the spelling
/// is inside and the location is not, and only one of the two can be quoted, so the answer is the
/// one the operating system would open.
///
/// **The two answers are the same kind of answer.** [`resolve_location`] falls back to the spelling
/// when nothing on a path resolves, and it is applied to both sides here, so a comparison never
/// holds one location against one spelling — which reads as a path being outside its own tree
/// wherever a platform spells a directory two ways, and is the failure this function exists to
/// make unrepresentable.
///
/// **Why the version this replaces could be wrong in both directions.** It canonicalised each side
/// and, if *either* answer came back an error, compared the two spellings as given. That fallback is
/// an over-approximation only when both sides fail, and only then: with both unresolved the
/// comparison is lexical, which says yes where the truth may be no. With *one* side resolved it
/// compares a location against a spelling, and a location is not written like a spelling, so it says
/// **no** where the truth is yes — for an index inside a repository that is reached through a second
/// name for it, the caller is told it is outside. That is the direction its own comment claimed it
/// could not get wrong, and it is a missed finding rather than a refusal: nothing in the engine
/// consults this to decide where to write, so the cost is a warning that does not appear.
///
/// It errs the other way for the same reason. A path spelled *under* the root but reaching outside
/// it is lexically inside, so the fallback called it inside where the answer is outside, and a
/// caller would tell a user to go and delete an in-repository index that is not there. Neither
/// direction is a security question; both are the same defect, which is comparing two answers that
/// are not the same kind of answer.
#[must_use]
pub fn contains(root: &Path, path: &Path) -> bool {
    relative_path(&resolve_location(root), &resolve_location(path)).is_some()
}

/// The components of `path` that follow `base`, or `None` when `path` does not begin with it.
///
/// **The one component walk in the crate.** Everything that answers a containment question goes
/// through here — [`relative_path`] is this on normalised spellings, and [`contains`] is
/// [`relative_path`] on located ones — so a caller cannot accept a path at a gate and then fail to
/// strip the base from it, and a test that pins one answer pins both.
///
/// **`Some` an empty path when `path` *is* the base**: there is nothing under the root, and that has
/// to be an answer rather than a failure.
///
/// **The comparison is exact, with the verbatim prefix excepted.** On Windows `canonicalize`
/// answers in the extended-length spelling, so a root the caller spelled as `C:\repo` and a
/// location the filesystem named as `\\?\C:\repo` are one directory in two spellings, and they agree
/// on every component except the first: `Prefix(VerbatimDisk(C))` against `Prefix(Disk(C))`.
/// [`Path::starts_with`] calls that a different directory, which would refuse every path inside the
/// repository and blame a symlink for it — a second thing which is not true.
///
/// **The prefix is the only exception, and that is the limit of what a comparison can do.** The next
/// difference on the same platform is a *name* — a short name the filesystem expands — and on macOS
/// it is a link in the middle of the path (`/var` for `/private/var`). A name carries a location,
/// so there is nothing here to fold: those are settled by asking the filesystem, in
/// [`resolve_location`], and not by deciding that two spellings are equal when they are not.
///
/// **Case is still exact, on purpose**: `docs/path-trust.md` records refusing `C:\Repo` against a
/// root of `C:\repo` as a false refusal it is choosing not to fix, and folding case here would
/// quietly fix it in one direction only.
fn strip_base(base: &Path, path: &Path) -> Option<PathBuf> {
    let mut expected = base.components();
    let mut inside: Vec<OsString> = Vec::new();
    for component in path.components() {
        match expected.next() {
            Some(want) if same_component(&want, &component) => {}
            // A component of `base` that `path` does not have: the two diverge here, so nothing
            // after this point is under the base.
            Some(_) => return None,
            None => inside.push(component.as_os_str().to_os_string()),
        }
    }
    // `path` ran out before `base` did. The base is longer than the path, so it is not under it and
    // there is nothing to strip.
    if expected.next().is_some() {
        return None;
    }
    Some(inside.into_iter().collect())
}

/// Whether two components of a path name the same thing.
///
/// **Equal, with the verbatim prefix excepted.** `\\?\C:\repo` and `C:\repo` are one directory in
/// two spellings, and the prefix is the only part of the difference that carries no location. Every
/// other prefix has to be equal on its own terms: a different drive designator is a different
/// volume, a different share is a different share, and a disk prefix is never a share prefix
/// however much alike the two spellings look.
fn same_component(left: &Component<'_>, right: &Component<'_>) -> bool {
    let (Component::Prefix(left), Component::Prefix(right)) = (left, right) else {
        return left == right;
    };
    match (left.kind(), right.kind()) {
        (Prefix::VerbatimDisk(disk), Prefix::Disk(other))
        | (Prefix::Disk(disk), Prefix::VerbatimDisk(other)) => disk == other,
        (Prefix::VerbatimUNC(server, share), Prefix::UNC(other_server, other_share)) => {
            server == other_server && share == other_share
        }
        (Prefix::UNC(server, share), Prefix::VerbatimUNC(other_server, other_share)) => {
            server == other_server && share == other_share
        }
        (left, right) => left == right,
    }
}

/// Whether `base` and `spelled` name the same volume: the component that says *where* a path is,
/// as against the names underneath it that say *which directory on it*.
///
/// **The volume is the one part of a path with no second spelling to reconcile.** Every component
/// below it can be an alias for somewhere else — a short name the filesystem expands, a directory
/// that is itself a link — so the filesystem is the only authority on those and this function is
/// only the one place that says so without asking. A prefix has two spellings and no two places
/// (`\\?\C:` and `C:`), which is what [`same_component`] reads, and which is what lets a path typed
/// in full reach the filesystem at all.
///
/// **This is what keeps a wrong path costing no I/O.** A share asked about against a drive-letter
/// root is two volumes, so `\\no-such-share\x.rs` is refused on the spelling and the network is
/// never asked; the same goes for a path that climbs, which is arithmetic and needs nothing.
#[must_use]
pub fn same_volume(base: &Path, spelled: &Path) -> bool {
    match (base.components().next(), spelled.components().next()) {
        (Some(base), Some(spelled)) => same_component(&base, &spelled),
        // Both are absolute by the time they arrive, so a missing first component is not reachable
        // and is not this function's to decide. The containment tests answer either way.
        _ => true,
    }
}

/// Whether `path` spells a `..` anywhere.
///
/// **The only way out that arithmetic can see, and the one refusal the filesystem does not get to
/// overturn.** A spelling without one cannot have climbed out of the root: every component is a
/// name under it, and resolution can only rewrite a name to what it points at, which the test on
/// the resolved location has already answered. So the question is left to the filesystem only where
/// the spelling had no climb to disagree with — and kept away from it where it did, because there
/// the refusal is a statement about what the path asked for rather than about where it landed.
///
/// Read off the spelling *as given*, not off the normalised path: normalisation has already spent a
/// `..` on the name before it, and spent it correctly.
#[must_use]
pub fn climbs(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

/// Collapse `.`, `//` and `..` without asking the filesystem anything.
///
/// `Path::components` already drops `.` and repeated separators, so only `..` is left: pop the
/// previous name, or do nothing at the filesystem root, which is what the filesystem does with a
/// `..` that has nowhere left to climb.
///
/// **Lexically, and that is a decision rather than a shortcut.** The platforms do not agree about
/// what `..` means behind a link. A kernel resolves it *after* following the link, so
/// `<root>/src/link/..` is the parent of the link's target and not the parent of `link`; Windows is
/// given a DOS path and takes the `..` off it before the filesystem is asked anything, so it
/// arrives here already collapsed. Only arithmetic is a reading both platforms share, and either way
/// both candidates are inside the root or both are outside it as far as containment goes — which is
/// why [`resolve_location`] asks the filesystem about the *spelling* and this function's answer is
/// only ever the gate.
#[must_use]
pub fn normalise_lexically(path: &Path) -> PathBuf {
    let mut kept: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(kept.last(), Some(Component::Normal(_))) {
                    kept.pop();
                } else if kept.is_empty() {
                    // A relative path with nothing in front of it: the `..` is the whole of what is
                    // known about where this is, and dropping it would move the path up a level.
                    kept.push(component);
                }
                // Otherwise the last thing kept is the filesystem root, which `..` names again.
            }
            other => kept.push(other),
        }
    }
    kept.into_iter().collect()
}

/// The location `path` reaches: its deepest existing prefix, resolved, with the names that follow
/// it on the end.
///
/// **Walking up one component at a time is what makes a path addressable when its parent directory
/// does not exist either.** A walk that tried the whole path, then stripped a single name and
/// canonicalised the parent, would be the same walk truncated after one step — so `<root>/a/b/c.rs`
/// with no `a` on disk would be a filesystem error rather than a path. Every step past the first is
/// what lets a nonexistent intermediate component be a name.
///
/// **On the spelling, not on the normalised path, and that is what makes the answer the right
/// file.** Normalising first would turn `<root>/src/link/../a.rs` into `<root>/src/a.rs` and lose
/// the link — and on a kernel, where the filesystem resolves the link and then climbs, that is a
/// different file from the one it opens. Which file it is differs by platform, and that is the
/// platform's to decide rather than this module's: on Windows the `..` comes off the DOS path
/// before the filesystem is asked, so the link is never followed and the answer is `src\a.rs`
/// there. Either way the answer is the location the operating system would open, which is what this
/// walk is asked for. The tail cannot reintroduce the same problem: the walk only stops where a
/// component is *missing*, and a missing component has no symlink to follow, so there is nothing
/// for a `..` in the tail to mean other than what it is read as here.
///
/// **A `..` is climbed past rather than spent, because it is arithmetic and not a name.** On a
/// kernel `realpath` needs every component to exist, so `<root>/src/never/../also-never.rs` has no
/// resolvable prefix and the walk would otherwise stop on the `..` and answer with the spelling —
/// which is a different *kind* of answer from the one the root gets, and the two cannot be
/// compared. Pushing the `..` onto the tail lets [`normalise_lexically`] spend it against the name in
/// front of it, so the walk reaches `<root>/src` and answers `<root>/src/also-never.rs`. On Windows
/// the same path resolves on the first try instead, because the DOS path parser has already taken
/// the `..` off it, so this branch is never reached there and Windows keeps the answer it had.
///
/// Falls back to the spelling when nothing on the path resolves, which is a statement about the
/// filesystem — a volume that is not mounted, a share that is not answering — and not about the
/// path. **The fallback is the same kind of answer on both sides**: a walk that resolves nothing
/// here stops at a filesystem root or a volume prefix, and the root — which is a prefix of every
/// anchored path — reaches one of those no later than the path does, so the two are either both
/// locations or both spellings.
///
/// **That is what makes it safe for the caller to let an answer overturn a refusal.** A walk that
/// resolved something answers with a location the filesystem named; a walk that named nothing
/// answers with the spelling it was handed, which is the very thing the gate had already tested and
/// rejected. So a resolution can turn a refusal into an answer only by producing one, and when it
/// cannot answer it says exactly what the gate said — which is also why no answer here depends on
/// which of the two spellings of a directory the caller happened to type.
#[must_use]
pub fn resolve_location(path: &Path) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(mut location) = cursor.canonicalize() {
            for name in tail.iter().rev() {
                location.push(name);
            }
            return normalise_lexically(&location);
        }
        // **A `..` is arithmetic rather than a name, and this walk has to climb past one without
        // spending it.** `file_name` answers `None` for a path that ends in `..`, and reading that
        // as *there is no name left to strip* ends the walk on the spot — with the spelling the
        // walk was handed, which is not a location and has never been asked about anything.
        //
        // That is not a corner of the contract. `<root>/src/never/../also-never.rs` has no existing
        // prefix below `src`, so a kernel's `realpath` cannot resolve anything on the path, and the
        // walk stopped there — returning `<root>/src/also-never.rs`, still spelled `/var/...`, to be
        // compared against a root that *had* resolved to `/private/var/...`. One side a location and
        // the other a spelling of the same place, which is the comparison this module must never
        // make: the CLI's discovery walk hit the same trap and answered it by having both sides come
        // from `canonicalize`.
        //
        // So the `..` goes onto the tail rather than being dropped: [`normalise_lexically`] below
        // spends it against the name in front of it, which is exactly what it does to the rest of
        // the path, and the walk carries on to the parts above. `<root>/src/never/../also-never.rs`
        // then resolves against `<root>/src` and comes back as `<root>/src/also-never.rs` — a
        // location, from a path no kernel would open.
        if cursor.components().next_back() == Some(Component::ParentDir) {
            let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
                return normalise_lexically(path);
            };
            tail.push(Component::ParentDir.as_os_str().to_os_string());
            cursor = parent;
            continue;
        }
        // **Only now is the walk out of road.** `file_name` answering `None` means a filesystem
        // root or a volume prefix and nothing else, which is what the comment on this branch used to
        // claim while the `..` case reached it as well.
        let Some(name) = cursor.file_name().map(std::ffi::OsStr::to_os_string) else {
            return normalise_lexically(path);
        };
        let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
            return normalise_lexically(path);
        };
        tail.push(name);
        cursor = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        climbs, contains, normalise_lexically, relative_path, resolve_location, same_volume,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A directory that removes itself, so a failing test leaves nothing behind.
    ///
    /// The label prefix is this module's own, because every unit test in the crate shares one
    /// process: two modules that built `<temp>/peek-<label>-<pid>-<n>` could collide and hand each
    /// other a directory that was already deleted.
    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "peek-contain-{}-{label}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create a temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Make a directory symlink.
    ///
    /// **Windows grants symlink creation to an elevated process or one in Developer Mode**, so a
    /// test that needs a link can be *unrunnable* there rather than failing. Every assertion in the
    /// two tests below is a statement about what a link does, so there is nothing left to assert
    /// without one — which is why those tests are compiled out on Windows rather than run and
    /// quietly skipped. The tests that need no link do run there.
    #[cfg(unix)]
    fn make_directory_symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create a directory symlink");
    }

    // -----------------------------------------------------------------------
    // The lexical rule, which is what two of the three callers use.
    // -----------------------------------------------------------------------

    #[test]
    fn a_climb_out_of_the_root_has_no_repository_relative_name() {
        // The shape the rule exists for, and the one a component-wise prefix comparison cannot see:
        // every component of `/repo/../escape.rs` is under `/repo`, and the `..` is the only thing
        // in it that says otherwise — which is why reading the spelling is not the same as reading
        // what the spelling says.
        let root = Path::new("/repo");
        let escape = root.join("../escape.rs");
        assert!(escape.starts_with(root), "under the root, component-wise");
        assert_eq!(relative_path(root, &escape), None);
        // And the spelling and the normalised spelling have to differ here, or the case is a
        // comparison of a path with itself.
        assert_ne!(normalise_lexically(&escape), escape, "not a spelling");
    }

    #[test]
    fn a_climb_that_stays_inside_the_root_is_the_path_it_lands_on() {
        // The other half, and the reason the rule is arithmetic rather than a refusal of `..`: a
        // climb that ends up inside the root is an ordinary path, and naming it is the whole job.
        let root = Path::new("/repo");
        let climbed = root.join("src/never/../also-never.rs");
        let landed = PathBuf::from("src/also-never.rs");
        assert_eq!(relative_path(root, &climbed), Some(landed));
    }

    #[test]
    fn a_shared_name_prefix_is_not_containment() {
        // `/x/repo-old` is not inside `/x/repo`. A string prefix says it is, which is the mistake
        // that would let one repository's rows be removed by a command aimed at another.
        let parent = Path::new("/x");
        let repo = parent.join("repo");
        let sibling = parent.join("repo-old/a.rs");
        assert_eq!(relative_path(&repo, &sibling), None);
        let inside = repo.join("src/a.rs");
        let named = PathBuf::from("src").join("a.rs");
        assert_eq!(relative_path(&repo, &inside), Some(named));
    }

    #[test]
    fn the_root_itself_has_an_empty_relative_name() {
        // There is nothing under the root, and that has to be an answer rather than a failure:
        // refusing it here is what turns `rm .` into a message about somewhere else.
        let root = Path::new("/repo");
        assert_eq!(relative_path(root, root), Some(PathBuf::new()));
        // And the other direction has to stay refused, or a parent would count as inside.
        assert_eq!(relative_path(root, Path::new("/")), None);
    }

    // -----------------------------------------------------------------------
    // The located rule, and the two ways the one-sided fallback used to answer.
    // -----------------------------------------------------------------------

    /// The fixture both located tests share: a repository and a second name for the whole directory.
    ///
    /// The precondition every test below depends on is that the two spellings differ **by
    /// construction**, on every platform: on Linux `std::env::temp_dir()` and its `canonicalize()`
    /// are the same string, so a fixture that derived both spellings from the temporary directory
    /// would be comparing a path with itself and would pass whatever the rule did. Every fixture
    /// here builds the second name with a symlink of its own, so the difference is a property of
    /// the test rather than of the machine it runs on.
    #[cfg(unix)]
    fn two_spellings_of_one_repository(label: &str) -> (Temp, Temp, PathBuf) {
        let tree = Temp::new(label);
        let home = Temp::new(&format!("{label}-alias"));
        let alias = home.path().join("by-another-name");
        make_directory_symlink(tree.path(), &alias);
        (tree, home, alias)
    }

    #[cfg(unix)]
    #[test]
    fn a_location_reached_through_a_second_name_for_the_root_is_inside_it() {
        // The false negative. The index is inside the repository and is spelled through a link to
        // it, so `canonicalize` cannot answer for it — the index directory is created on first use,
        // and this one was never created. The rule this replaces compared that unresolved spelling
        // against a resolved root and said *outside*, so a caller asking where the index lives was
        // told it was somewhere it was not, and stayed silent about an index inside the repository.
        let (tree, _home, alias) = two_spellings_of_one_repository("inside-by-another-name");
        let index = alias.join(".peek/index.db");

        // **Both preconditions asserted.** The leaf does not exist, so the one side that could
        // resolve did not; and the spelling does not begin with the root, so a comparison of the
        // two raw spellings says no. Without the first this would be a comparison of two locations,
        // and without the second it would pass whatever the rule did.
        assert!(!index.exists(), "nothing is there to resolve");
        assert!(!index.starts_with(tree.path()), "spellings must differ");
        assert!(contains(tree.path(), &index), "inside the repository");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_repository_is_not_the_repository_however_it_is_spelled() {
        // The false positive, and the other half of the same rule. The spelling *is* under the root,
        // and so a comparison of two raw spellings says inside — for an index that lives somewhere
        // else entirely. A caller asking where the index lives would tell a user to go and delete
        // an in-repository index that does not exist.
        let tree = Temp::new("escape-by-spelling");
        let elsewhere = Temp::new("escape-by-spelling-target");
        let link = tree.path().join("link");
        make_directory_symlink(elsewhere.path(), &link);
        let index = link.join("index.db");

        // The precondition, asserted: the spelling reads as inside, which is the claim being
        // contradicted rather than assumed.
        assert!(index.starts_with(tree.path()), "reads as inside");
        assert!(!contains(tree.path(), &index), "the link leaves the tree");
    }

    #[test]
    fn the_repository_contains_its_own_files_and_nothing_above_itself() {
        // The two answers that must not move while the two above change: the ordinary case, and the
        // boundary. Both are measured against a fixture on disk, so they also pin that `contains`
        // is not vacuously true.
        let tree = Temp::new("ordinary");
        std::fs::create_dir_all(tree.path().join("src")).expect("create the source directory");
        std::fs::write(tree.path().join("src/a.rs"), "fn a() {}\n").expect("write source");
        let file = tree.path().join("src/a.rs");
        let parent = tree.path().parent().expect("a temp dir has a parent");

        assert!(contains(tree.path(), tree.path()), "inside itself");
        assert!(contains(tree.path(), &file), "a file in the tree");
        assert!(!contains(tree.path(), parent), "a parent is outside");
    }

    // -----------------------------------------------------------------------
    // The mechanism, moved here with the rule it implements.
    // -----------------------------------------------------------------------

    #[test]
    fn lexical_normalisation_is_arithmetic_and_needs_no_filesystem() {
        // The gate's own rules, on their own, including the two that are easy to get backwards: a
        // leading `..` on a *relative* path is kept, because dropping it would move the path up a
        // level, and a `..` at a filesystem root is dropped, because the filesystem agrees and
        // keeping it would make `/..` look like an escape.
        assert_eq!(
            normalise_lexically(Path::new("a/./b")),
            PathBuf::from("a/b")
        );
        assert_eq!(normalise_lexically(Path::new("a//b")), PathBuf::from("a/b"));
        assert_eq!(
            normalise_lexically(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            normalise_lexically(Path::new("../a/../b")),
            PathBuf::from("../b")
        );
        assert_eq!(normalise_lexically(Path::new(".")), PathBuf::from(""));
        let root = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(normalise_lexically(&root.join("a/../..")), root);
    }

    #[test]
    fn the_question_of_which_paths_the_filesystem_is_asked_about_is_arithmetic() {
        // The two preconditions that decide whether a refusal on the spelling is final, and they are
        // pure string judgements, so they are checked on whichever host runs the suite rather than
        // only on the ones where they have consequences.
        //
        // **A `..` is read off the spelling as given, not off the normalised path**, and that is the
        // half that is easy to get backwards: normalisation has already spent the climb on the name
        // before it, so `a/b/../c` reads as no climb and `a/../..` reads as one that goes nowhere —
        // which is a different question from whether it climbs *out*, and is not this function's.
        assert!(climbs(Path::new("a/../b")), "a climb anywhere is a climb");
        assert!(climbs(Path::new("a/b/../../c")));
        assert!(climbs(Path::new("a/..")));
        assert!(
            !climbs(Path::new("a/b/c")),
            "a path with no `..` in it cannot have climbed out of anything"
        );
        assert!(!climbs(Path::new("")), "an empty path climbs nowhere");
        assert!(
            !climbs(Path::new("a/..b/c")),
            "`..b` is a name that begins with two dots, not a climb, and reading it as one would \
             refuse a legitimate directory"
        );

        // **Same volume means the same first component**, which on a path that has a prefix is the
        // prefix and on one that has not is the root directory. Both spellings of one prefix agree
        // here, and that is what lets a path typed in full reach the filesystem at all.
        let root = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
        let repository = root.join("repo");
        assert!(
            same_volume(&repository, &repository.join("src/a.rs")),
            "a path inside the repository is on the repository's volume"
        );
        assert!(
            same_volume(&repository, &root),
            "the root itself is on its own volume, whatever else is under it"
        );
        assert!(
            !same_volume(Path::new("C:\\repo"), Path::new("D:\\repo\\a.rs")),
            "two paths whose first components differ are two volumes: on Windows two drive letters \
             whatever else the spellings share, and on Unix two different first names. A caller only \
             ever passes absolute paths, where the first component on Unix is the root directory \
             and always agrees — so what this pins here is the walk rather than a case it can reach"
        );
    }

    /// The path `canonical` names, spelled without the verbatim prefix — the one spelling
    /// difference that carries no location.
    ///
    /// **Both spellings are derived from one canonical path on purpose, and that is the whole
    /// point of it.** Reaching for one directory two ways and assuming the two spellings differ in
    /// their prefix and nothing else makes the assertion a claim about the machine the suite runs
    /// on, and it is false on both platforms CI runs. A Windows profile with a short name differs
    /// in a *component* as well as in the prefix (`RUNNER~1` against `runneradmin`), and a macOS
    /// temporary directory differs by `/var` against `/private/var`. Neither difference is the
    /// prefix, and neither is something the prefix exception is for — so a fixture that inherited
    /// either of them would fail for a second reason, and the failure read as a bug in the walk
    /// rather than as the fact that the fixture was wrong.
    ///
    /// Deriving both from one canonical path makes them differ in the prefix by construction, and
    /// the test below then holds on a machine with a short-named profile or a symlinked temporary
    /// directory as well as on one without. The *other* differences are not waved away — they are
    /// what the located rule settles by asking the filesystem, and what
    /// `a_location_reached_through_a_second_name_for_the_root_is_inside_it` asserts on every
    /// platform.
    fn without_the_verbatim_prefix(canonical: &Path) -> PathBuf {
        let verbatim = canonical.to_string_lossy();
        if let Some(share) = verbatim.strip_prefix(r"\\?\UNC\") {
            // A share's verbatim spelling is a UNC path with a prefix in the middle of it, so
            // taking the prefix off is not the same operation as on a drive.
            PathBuf::from(format!(r"\\{share}"))
        } else if let Some(bare) = verbatim.strip_prefix(r"\\?\") {
            PathBuf::from(bare)
        } else {
            // Nothing to take off: a path with no prefix component, where the canonical spelling is
            // the only spelling there is, and the pair under test is the path against itself.
            canonical.to_path_buf()
        }
    }

    #[test]
    fn containment_is_decided_on_components_rather_than_on_a_spelling() {
        // The judgement every answer in this module rests on, exercised on two spellings of one
        // directory. On Windows `canonicalize` answers `\\?\C:\repo` where the caller spelled
        // `C:\repo`, so the two spellings are the ordinary state of affairs there rather than an
        // edge case, and a string comparison refuses every path inside the repository in exchange —
        // with a message that blames a symlink for it.
        //
        // **The pair is built by this test rather than taken from the machine**, because the machine
        // does not reliably have one. A path with a short name in it, or under a directory that is
        // itself a link, differs from its canonical form in a component that carries a location,
        // and that is not what the exception in `strip_base` is for — so a fixture that inherited
        // such a difference would be asserting something else and failing for a second reason. See
        // [`without_the_verbatim_prefix`].
        //
        // **On a host whose paths carry no prefix there is no second spelling to hand out**, so the
        // pair is the path against itself and what is asserted is the walk's self-consistency plus
        // the boundaries below. The verbatim spelling is then covered by the tests that go through a
        // located answer, which reach it wherever the platform can: a helper that can only be
        // exercised on one platform is not a test.
        //
        // Nothing here touches the disk beyond creating the fixture. These are judgements about two
        // spellings, and a test that needs them to exist is testing the filesystem.
        let directory = Temp::new("under");
        std::fs::create_dir_all(directory.path().join("src")).expect("create src");
        let canonical = directory.path().canonicalize().expect("canonicalise");
        let spelled = without_the_verbatim_prefix(&canonical);
        let named = canonical.join("src").join("a.rs");
        let typed = spelled.join("src").join("a.rs");
        let above = canonical.parent().expect("a temp directory has a parent");
        // `/x/repo` and `/x/repo-old`, which is the shape the gate exists to refuse.
        let repo = above.join("repo");
        let repo_old = above.join("repo-old/a.rs");
        let expected = PathBuf::from("src").join("a.rs");
        let from_named = Some(expected.clone());
        let from_typed = Some(expected);
        let nothing = PathBuf::new();
        let under_the_root = canonical
            .strip_prefix(above)
            .expect("the canonical path is under its parent")
            .to_path_buf();

        // **The fixture's own precondition, asserted rather than assumed**: past the first component
        // the two spellings are the same components, so everything below is about the prefix and
        // about nothing else.
        assert!(
            canonical
                .components()
                .zip(spelled.components())
                .skip(1)
                .all(|(named, spelled)| named == spelled),
            "the two spellings must differ in the prefix alone, or this test is about the \
             machine's spelling rather than about the prefix: {} and {}",
            canonical.display(),
            spelled.display()
        );

        // **Both directions**, because a comparison with the arguments the wrong way round refuses
        // almost everything and looks, from the failure, like a containment bug in the caller
        // rather than in the direction: a location the filesystem named against a root the caller
        // spelled, and the same pair the other way round, which is the direction a gate asks in.
        // A location and a root that name one directory in two spellings are one directory, so the
        // answer is the same path the index holds either way.
        assert_eq!(relative_path(&spelled, &named), from_named, "one directory");
        // The other direction is the gate's own: a root the filesystem named against a path typed
        // in full, which is the pair a caller actually produces.
        assert_eq!(relative_path(&canonical, &typed), from_typed, "the same");

        // **What the exception is not.** A verbatim prefix carries no location, so it is read as
        // the bare prefix it stands for; a *name* carries one, so it is compared as written. This
        // is the boundary a reader is most likely to widen by accident, and it is the same boundary
        // `docs/path-trust.md` draws for case: an answer about the wrong case is a false refusal,
        // which is a different thing from an escape and is a separate decision.
        //
        // **Two names differing in nothing but case, on purpose.** On Windows and macOS these are
        // one directory, so a reader who has just been shown the prefix folded has every reason to
        // expect this folded too. It is not, and the comparison is a comparison of spellings: which
        // is exactly why the file does not have to exist for this to be an answer, and why the
        // refusal it produces is recorded in `docs/path-trust.md` as a decision rather than an
        // oversight.
        let upper_case = above.join("Repo");
        let lower_case = above.join("repo");
        let miscased = lower_case.join("a.rs");
        assert_eq!(relative_path(&upper_case, &miscased), None, "case is exact");

        assert_eq!(relative_path(&canonical, &canonical), Some(nothing));
        assert_eq!(relative_path(&above, &canonical), Some(under_the_root));
        assert_eq!(relative_path(&canonical, above), None, "outside the tree");
        assert_eq!(relative_path(&repo, &repo_old), None, "a shared prefix");
    }

    #[cfg(unix)]
    #[test]
    fn a_climb_the_walk_cannot_resolve_still_answers_with_a_location() {
        // **The walk's own contract, on the one shape that broke it, and checked here rather than
        // through a caller because the two failures it caused were indistinguishable from the
        // outside.**
        //
        // A walk that cannot resolve anything used to answer with the path's own spelling, while the
        // root's walk answered with a location. One directory compared with itself in two forms is
        // a comparison with no answer, and where the platform spells a temporary directory two ways
        // — `/var` for the caller, `/private/var` for the resolver — it reads as the path being
        // outside the tree and blames a symlink for it. That is a second thing which is not true.
        //
        // **A link rather than the platform's own alias, so the case is not left to the machine.**
        // On a runner whose temporary directory is already a link this happens with no fixture at
        // all; on one where it is not, nothing happens and the property goes untested. Reaching the
        // repository through a link makes one directory two spellings on every platform, including
        // the one that needs it least — which is the point, because a test that only bites where the
        // bug already bites is not a test.
        let parent = Temp::new("walk-climb");
        let root = parent.path().join("repo");
        std::fs::create_dir_all(&root).expect("create the repository");
        let alias = parent.path().join("alias");
        make_directory_symlink(&root, &alias);

        // **The climb is left written down**, so the walk has to spend it rather than hand back the
        // path it was given. `never` is absent, which is what makes the tail unresolvable and sends
        // a kernel's `realpath` down this branch at all.
        let climbed = alias.join("src/never/../also-never.rs");
        let expected = root
            .canonicalize()
            .expect("canonicalise")
            .join("src")
            .join("also-never.rs");

        // **The fixture's own precondition, asserted rather than assumed**: a spelling read purely
        // lexically is *not* the answer, so this cannot pass by handing the path straight back.
        assert_ne!(
            normalise_lexically(&climbed),
            expected,
            "the spelling and the location have to differ here, or this asserts nothing about the \
             walk having resolved anything: {} and {}",
            normalise_lexically(&climbed).display(),
            expected.display()
        );

        assert_eq!(
            resolve_location(&climbed),
            expected,
            "the walk spends the `..` against the name in front of it and carries on to the parts \
             above, so a climb that blocks resolution still answers with the location the \
             filesystem named -- {}",
            climbed.display()
        );
    }
}