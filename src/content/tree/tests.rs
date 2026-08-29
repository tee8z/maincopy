#![cfg(target_os = "linux")]

use std::{
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt as _, fs::symlink},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use rustix::{
    fs::{CWD, Mode, mkfifoat},
    io::Errno,
};
use tempfile::{TempDir, tempdir};

use super::*;
use crate::content::{ContentValidationCode, DraftStatus};

const PUBLICATION: &str = "[site]\n\
title = \"Tree Tests\"\n\
base_url = \"https://tree.example.test\"\n\
description = \"Safe content tree tests.\"\n\
\n\
[author]\n\
name = \"Tree Tester\"\n";

const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";
const THIRD_ID: &str = "33333333-3333-4333-8333-333333333333";

fn post_source(id: &str, slug: &str, draft: Option<bool>) -> String {
    let draft = match draft {
        Some(value) => format!("draft = {value}\n"),
        None => String::new(),
    };
    format!(
        "+++\n\
         id = \"{id}\"\n\
         title = \"Post {slug}\"\n\
         slug = \"{slug}\"\n\
         authored_at = 2026-08-29T12:00:00Z\n\
         description = \"Description for {slug}.\"\n\
         {draft}\
         +++\n\
         # {slug}\n"
    )
}

fn new_root() -> TempDir {
    let root = tempdir().expect("temporary content root must be created");
    write(root.path(), "publication.toml", PUBLICATION.as_bytes());
    root
}

fn write(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent directories must be created");
    }
    fs::write(&path, bytes).expect("fixture file must be written");
    path
}

fn limits(
    publication: u64,
    post: u64,
    asset: u64,
    tree: u64,
    entries: usize,
    depth: usize,
    path: usize,
) -> ContentTreeLimits {
    ContentTreeLimits::new(
        ContentFileByteLimit::new(publication).expect("publication limit must be nonzero"),
        ContentFileByteLimit::new(post).expect("post limit must be nonzero"),
        ContentFileByteLimit::new(asset).expect("asset limit must be nonzero"),
        ContentTreeByteLimit::new(tree).expect("tree limit must be nonzero"),
        ContentEntryLimit::new(entries).expect("entry limit must be nonzero"),
        ContentDepthLimit::new(depth).expect("depth limit must be nonzero"),
        ContentPathByteLimit::new(path).expect("path limit must be nonzero"),
    )
    .expect("fixture limits must form a valid contract")
}

#[derive(Debug, Eq, PartialEq)]
struct ErrorContract {
    path: String,
    code: ContentValidationCode,
}

fn error_contract(
    result: Result<DiscoveredContentTree, ContentValidationErrors>,
) -> Vec<ErrorContract> {
    validation_error_contract(result.expect_err("fixture discovery must fail"))
}

fn validation_error_contract(errors: ContentValidationErrors) -> Vec<ErrorContract> {
    errors
        .into_errors()
        .into_iter()
        .map(|error| ErrorContract {
            path: error.path().as_str().to_owned(),
            code: error.code(),
        })
        .collect()
}

fn has_error(errors: &[ErrorContract], path: &str, code: ContentValidationCode) -> bool {
    errors
        .iter()
        .any(|error| error.path == path && error.code == code)
}

#[test]
fn discovers_nested_tree_in_order_owns_bytes_and_validates() {
    let root = new_root();
    let zeta = post_source(THIRD_ID, "zeta", None);
    let draft = post_source(FIRST_ID, "later", None);
    let alpha = post_source(SECOND_ID, "alpha", None);
    let opaque = [0_u8, 0xff, 0x80, b'\n'];

    write(root.path(), "assets/z-last.bin", b"z");
    write(root.path(), "posts/guides/zeta.md", zeta.as_bytes());
    write(root.path(), "assets/nested/a.bin", &opaque);
    write(root.path(), "drafts/notes/later.md", draft.as_bytes());
    write(root.path(), "posts/alpha.md", alpha.as_bytes());

    let expected_total =
        PUBLICATION.len() + zeta.len() + draft.len() + alpha.len() + opaque.len() + 1;
    let tree = discover_content_tree(root.path(), ContentTreeLimits::default())
        .expect("valid nested tree must load");

    assert_eq!(
        tree.posts()
            .iter()
            .map(|post| post.path().as_str())
            .collect::<Vec<_>>(),
        [
            "drafts/notes/later.md",
            "posts/alpha.md",
            "posts/guides/zeta.md",
        ]
    );
    assert_eq!(
        tree.assets()
            .iter()
            .map(|asset| asset.path().as_str())
            .collect::<Vec<_>>(),
        ["assets/nested/a.bin", "assets/z-last.bin"]
    );
    assert_eq!(tree.assets()[0].bytes(), opaque);
    assert_eq!(tree.total_bytes().get(), expected_total as u64);

    write(
        root.path(),
        "assets/nested/a.bin",
        b"changed after discovery",
    );
    write(root.path(), "posts/alpha.md", b"not valid Markdown");
    write(root.path(), "publication.toml", b"not valid TOML");
    assert_eq!(tree.assets()[0].bytes(), opaque);

    let validated = tree
        .validate()
        .expect("owned source snapshot must validate");
    let loaded_draft = validated
        .posts()
        .iter()
        .find(|post| post.path().as_str() == "drafts/notes/later.md")
        .expect("draft-directory post must be present");
    assert_eq!(loaded_draft.metadata().draft(), DraftStatus::Draft);
}

#[test]
fn publication_only_tree_ignores_unmanaged_root_entries() {
    let root = new_root();
    write(root.path(), "README.md", b"repository notes");
    write(root.path(), ".git/objects/ignored", b"Git internals");
    symlink("missing", root.path().join("unmanaged-link"))
        .expect("unmanaged root symlink must be created");

    let tree = discover_content_tree(root.path(), ContentTreeLimits::default())
        .expect("unmanaged root entries must not enter content discovery");
    assert!(tree.posts().is_empty());
    assert!(tree.assets().is_empty());
    assert_eq!(tree.total_bytes().get(), PUBLICATION.len() as u64);

    let strict_tree = discover_content_tree(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            1,
            1,
            PUBLICATION.len() as u64,
            1,
            1,
            "publication.toml".len(),
        ),
    )
    .expect("unmanaged root entries must not consume the managed-entry limit");
    assert_eq!(strict_tree.total_bytes().get(), PUBLICATION.len() as u64);
}

#[test]
fn rejects_missing_roots_publication_and_invalid_managed_namespaces() {
    let missing_parent = tempdir().expect("missing-root parent must be created");
    let missing_errors = error_contract(discover_content_tree(
        &missing_parent.path().join("missing"),
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &missing_errors,
        "<content-root>",
        ContentValidationCode::ContentRootUnavailable,
    ));

    let file_root = write(missing_parent.path(), "not-a-directory", b"file");
    let file_root_errors = error_contract(discover_content_tree(
        &file_root,
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &file_root_errors,
        "<content-root>",
        ContentValidationCode::ContentRootUnavailable,
    ));

    let no_publication = tempdir().expect("empty content root must be created");
    let missing_publication = error_contract(discover_content_tree(
        no_publication.path(),
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &missing_publication,
        "publication.toml",
        ContentValidationCode::PublicationFileMissing,
    ));

    let invalid = new_root();
    fs::create_dir(invalid.path().join("Assets"))
        .expect("reserved case-variant fixture must be created");
    write(invalid.path(), "posts", b"not a directory");
    symlink("missing-assets", invalid.path().join("assets"))
        .expect("managed namespace symlink must be created");
    let errors = error_contract(discover_content_tree(
        invalid.path(),
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &errors,
        "Assets",
        ContentValidationCode::LogicalPathCaseCollision,
    ));
    assert!(has_error(
        &errors,
        "posts",
        ContentValidationCode::ContentNamespaceInvalid,
    ));
    assert!(has_error(
        &errors,
        "assets",
        ContentValidationCode::ContentSymlinkUnsupported,
    ));
}

#[test]
fn rejects_non_utf8_publication_and_post_text() {
    let root = new_root();
    fs::write(root.path().join("publication.toml"), [0xff])
        .expect("invalid publication fixture must be written");
    write(root.path(), "posts/invalid.md", &[0xff]);

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    for path in ["posts/invalid.md", "publication.toml"] {
        assert!(has_error(
            &errors,
            path,
            ContentValidationCode::ContentTextInvalidUtf8,
        ));
    }
}

#[test]
fn logical_asset_paths_reject_platform_and_encoded_traversal_forms() {
    assert_eq!(
        LogicalAssetPath::parse("assets/images/cover-v1.webp")
            .expect("portable asset path must parse")
            .as_str(),
        "assets/images/cover-v1.webp"
    );
    for path in [
        "",
        "/assets/file.bin",
        "../assets/file.bin",
        "assets/../file.bin",
        "assets/./file.bin",
        "assets//file.bin",
        "assets\\file.bin",
        "C:\\assets\\file.bin",
        "assets/%2e%2e/file.bin",
        "assets/%252e%252e/file.bin",
        "assets/café.bin",
        "posts/file.bin",
        "assets",
    ] {
        assert!(
            LogicalAssetPath::parse(path).is_err(),
            "unsafe logical path unexpectedly parsed: {path}"
        );
    }
}

#[test]
fn exact_file_tree_entry_depth_and_path_limits_are_inclusive() {
    let root = new_root();
    let post = post_source(FIRST_ID, "exact", None);
    let asset = b"bytes";
    write(root.path(), "posts/p.md", post.as_bytes());
    write(root.path(), "assets/a.bin", asset);

    let total = PUBLICATION.len() + post.len() + asset.len();
    let exact = limits(
        PUBLICATION.len() as u64,
        post.len() as u64,
        asset.len() as u64,
        total as u64,
        5,
        2,
        "publication.toml".len(),
    );
    let tree = discover_content_tree(root.path(), exact)
        .expect("values exactly equal to every limit must be accepted");
    assert_eq!(tree.total_bytes().get(), total as u64);
}

#[test]
fn limit_types_reject_zero_overflow_sentinels_and_invalid_relationships() {
    assert!(ContentFileByteLimit::new(0).is_none());
    assert!(ContentFileByteLimit::new(u64::MAX).is_none());
    assert!(ContentTreeByteLimit::new(0).is_none());
    assert!(ContentTreeByteLimit::new(u64::MAX).is_none());
    assert!(ContentEntryLimit::new(0).is_none());
    assert!(ContentDepthLimit::new(0).is_none());
    assert!(ContentPathByteLimit::new(0).is_none());

    let result = ContentTreeLimits::new(
        ContentFileByteLimit::new(2).expect("file limit must be valid"),
        ContentFileByteLimit::new(1).expect("file limit must be valid"),
        ContentFileByteLimit::new(1).expect("file limit must be valid"),
        ContentTreeByteLimit::new(1).expect("tree limit must be valid"),
        ContentEntryLimit::new(1).expect("entry limit must be valid"),
        ContentDepthLimit::new(1).expect("depth limit must be valid"),
        ContentPathByteLimit::new(1).expect("path limit must be valid"),
    );
    assert_eq!(result, Err(ContentTreeLimitsError));

    let defaults = ContentTreeLimits::default();
    assert_eq!(defaults.publication_file_bytes().get(), 256 * 1024);
    assert_eq!(defaults.post_file_bytes().get(), 4 * 1024 * 1024);
    assert_eq!(defaults.asset_file_bytes().get(), 32 * 1024 * 1024);
    assert_eq!(defaults.total_tree_bytes().get(), 256 * 1024 * 1024);
    assert_eq!(defaults.entries().get(), 10_000);
    assert_eq!(defaults.depth().get(), 16);
    assert_eq!(defaults.path_bytes().get(), 1_024);
}

#[test]
fn rejects_each_file_class_one_byte_over_its_limit() {
    let root = new_root();
    let post = post_source(FIRST_ID, "large", None);
    let asset = b"asset";
    write(root.path(), "posts/large.md", post.as_bytes());
    write(root.path(), "assets/large.bin", asset);

    let errors = error_contract(discover_content_tree(
        root.path(),
        limits(
            (PUBLICATION.len() - 1) as u64,
            (post.len() - 1) as u64,
            (asset.len() - 1) as u64,
            (PUBLICATION.len() + post.len() + asset.len()) as u64,
            16,
            8,
            256,
        ),
    ));

    for path in ["assets/large.bin", "posts/large.md", "publication.toml"] {
        assert!(
            has_error(&errors, path, ContentValidationCode::ContentFileTooLarge),
            "missing file-size diagnostic for {path}: {errors:?}"
        );
    }
}

#[test]
fn rejects_complete_tree_one_byte_over_its_limit() {
    let root = new_root();
    let post = post_source(FIRST_ID, "total", None);
    let asset = b"asset";
    write(root.path(), "posts/total.md", post.as_bytes());
    write(root.path(), "assets/total.bin", asset);
    let total = PUBLICATION.len() + post.len() + asset.len();

    let errors = error_contract(discover_content_tree(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            post.len() as u64,
            asset.len() as u64,
            (total - 1) as u64,
            16,
            8,
            256,
        ),
    ));
    assert!(
        errors
            .iter()
            .any(|error| { error.code == ContentValidationCode::ContentTreeTooLarge })
    );
}

#[test]
fn rejects_entry_depth_and_path_overflows() {
    let entries_root = new_root();
    write(entries_root.path(), "assets/a.bin", b"a");
    write(entries_root.path(), "assets/b.bin", b"b");
    write(entries_root.path(), "assets/c.bin", b"c");
    let entry_errors = error_contract(discover_content_tree(
        entries_root.path(),
        limits(1_024, 1_024, 1_024, 4_096, 2, 8, 256),
    ));
    assert!(
        entry_errors
            .iter()
            .any(|error| { error.code == ContentValidationCode::ContentEntryLimitExceeded })
    );

    let depth_root = new_root();
    write(depth_root.path(), "assets/one/file.bin", b"x");
    let depth_errors = error_contract(discover_content_tree(
        depth_root.path(),
        limits(1_024, 1_024, 1_024, 4_096, 16, 2, 256),
    ));
    assert!(has_error(
        &depth_errors,
        "assets/one/file.bin",
        ContentValidationCode::ContentDepthLimitExceeded,
    ));

    let path_root = new_root();
    let too_long = "assets/long-name.bin";
    write(path_root.path(), too_long, b"x");
    let path_errors = error_contract(discover_content_tree(
        path_root.path(),
        limits(1_024, 1_024, 1_024, 4_096, 16, 8, too_long.len() - 1),
    ));
    assert!(has_error(
        &path_errors,
        too_long,
        ContentValidationCode::ContentPathTooLong,
    ));
}

#[test]
fn configured_path_limit_above_the_default_is_used_end_to_end() {
    let root = new_root();
    let component = "a".repeat(200);
    let mut segments = vec!["assets".to_owned()];
    segments.extend(std::iter::repeat_n(component, 6));
    segments.push("file.bin".to_owned());
    let logical_path = segments.join("/");
    assert!(logical_path.len() > DEFAULT_PATH_BYTES);
    write(root.path(), &logical_path, b"asset");

    let tree = discover_content_tree(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            1,
            5,
            (PUBLICATION.len() + 5) as u64,
            16,
            8,
            logical_path.len(),
        ),
    )
    .expect("the configured path limit must govern discovery and asset construction");
    assert_eq!(tree.assets()[0].path().as_str(), logical_path);
}

#[test]
fn entry_limit_bounds_invalid_managed_names_before_diagnostic_fanout() {
    let root = new_root();
    for name in ["bad%one", "bad%two", "bad%three"] {
        write(root.path(), &format!("assets/{name}"), b"x");
    }

    let errors = error_contract(discover_content_tree(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            1,
            1,
            (PUBLICATION.len() + 3) as u64,
            4,
            2,
            128,
        ),
    ));
    assert_eq!(
        errors,
        [ErrorContract {
            path: "assets".to_owned(),
            code: ContentValidationCode::ContentEntryLimitExceeded,
        }]
    );
}

#[test]
fn rejects_non_utf8_non_ascii_and_percent_encoded_names() {
    let root = new_root();
    let assets = root.path().join("assets");
    fs::create_dir(&assets).expect("assets directory must be created");
    fs::write(assets.join("café.bin"), b"unicode").expect("Unicode fixture must be written");
    fs::write(assets.join("%2e%2e.bin"), b"encoded").expect("percent fixture must be written");
    let invalid = assets.join(OsString::from_vec(vec![0xff, b'.', b'b', b'i', b'n']));
    fs::write(invalid, b"invalid UTF-8").expect("non-UTF-8 fixture must be written");

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &errors,
        "assets/%2e%2e.bin",
        ContentValidationCode::InvalidLogicalContentPath,
    ));
    assert!(has_error(
        &errors,
        "assets/café.bin",
        ContentValidationCode::InvalidLogicalContentPath,
    ));
    assert!(errors.iter().any(|error| {
        error.path == "assets/<non-utf8-ff2e62696e>"
            && error.code == ContentValidationCode::UnsupportedFilenameEncoding
    }));
}

#[test]
fn rejects_case_collisions_for_files_and_directory_prefixes() {
    let root = new_root();
    write(root.path(), "assets/Photo.bin", b"first");
    write(root.path(), "assets/photo.BIN", b"second");
    write(root.path(), "assets/Folder/one.bin", b"first prefix");
    write(root.path(), "assets/folder/two.bin", b"second prefix");

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    assert!(has_error(
        &errors,
        "assets/photo.BIN",
        ContentValidationCode::LogicalPathCaseCollision,
    ));
    assert!(has_error(
        &errors,
        "assets/folder",
        ContentValidationCode::LogicalPathCaseCollision,
    ));
}

#[test]
fn rejects_all_authored_svg_filename_variants() {
    let root = new_root();
    for path in ["assets/a.svg", "assets/b.SVG", "assets/c.svgz"] {
        write(root.path(), path, b"<svg/>");
    }

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    for path in ["assets/a.svg", "assets/b.SVG", "assets/c.svgz"] {
        assert!(has_error(
            &errors,
            path,
            ContentValidationCode::AuthoredSvgUnsupported,
        ));
    }
}

#[test]
fn rejects_internal_external_and_broken_file_and_directory_symlinks() {
    let root = new_root();
    let external = tempdir().expect("external fixture root must be created");
    write(root.path(), "assets/real-file.bin", b"inside");
    fs::create_dir_all(root.path().join("assets/real-dir"))
        .expect("internal directory target must be created");
    write(external.path(), "outside.bin", b"outside");
    fs::create_dir(external.path().join("outside-dir"))
        .expect("external directory target must be created");

    symlink(
        "real-file.bin",
        root.path().join("assets/internal-file.bin"),
    )
    .expect("internal file symlink must be created");
    symlink(
        external.path().join("outside.bin"),
        root.path().join("assets/external-file.bin"),
    )
    .expect("external file symlink must be created");
    symlink(
        "missing-file.bin",
        root.path().join("assets/broken-file.bin"),
    )
    .expect("broken file symlink must be created");
    symlink("real-dir", root.path().join("assets/internal-dir"))
        .expect("internal directory symlink must be created");
    symlink(
        external.path().join("outside-dir"),
        root.path().join("assets/external-dir"),
    )
    .expect("external directory symlink must be created");
    symlink("missing-dir", root.path().join("assets/broken-dir"))
        .expect("broken directory symlink must be created");

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    for path in [
        "assets/broken-dir",
        "assets/broken-file.bin",
        "assets/external-dir",
        "assets/external-file.bin",
        "assets/internal-dir",
        "assets/internal-file.bin",
    ] {
        assert!(
            has_error(
                &errors,
                path,
                ContentValidationCode::ContentSymlinkUnsupported
            ),
            "missing symlink diagnostic for {path}: {errors:?}"
        );
    }
}

#[test]
fn rejects_fifo_and_unix_socket_without_blocking() {
    let root = new_root();
    let assets = root.path().join("assets");
    fs::create_dir(&assets).expect("assets directory must be created");
    mkfifoat(CWD, assets.join("pipe.bin"), Mode::RUSR | Mode::WUSR)
        .expect("FIFO fixture must be created");
    let _socket = std::os::unix::net::UnixListener::bind(assets.join("content.sock"))
        .expect("Unix socket fixture must be created");

    let root_path = root.path().to_owned();
    let (sender, receiver) = mpsc::channel();
    let task = thread::spawn(move || {
        let _ = sender.send(discover_content_tree(
            &root_path,
            ContentTreeLimits::default(),
        ));
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("special-file discovery must not block");
    task.join().expect("discovery task must not panic");

    let errors = error_contract(result);
    for path in ["assets/content.sock", "assets/pipe.bin"] {
        assert!(has_error(
            &errors,
            path,
            ContentValidationCode::UnsupportedContentEntryKind,
        ));
    }
}

#[test]
fn drafts_missing_or_true_are_forced_draft_and_false_is_rejected() {
    let accepted = new_root();
    let missing = post_source(FIRST_ID, "missing-draft", None);
    let explicit = post_source(SECOND_ID, "explicit-draft", Some(true));
    write(accepted.path(), "drafts/missing.md", missing.as_bytes());
    write(accepted.path(), "drafts/true.md", explicit.as_bytes());
    let tree = discover_content_tree(accepted.path(), ContentTreeLimits::default())
        .expect("valid drafts must be discovered");
    let validated = tree.validate().expect("valid drafts must validate");
    assert_eq!(validated.posts().len(), 2);
    assert!(
        validated
            .posts()
            .iter()
            .all(|post| post.metadata().draft() == DraftStatus::Draft)
    );

    let rejected = new_root();
    let explicit_false = post_source(THIRD_ID, "false-draft", Some(false));
    write(
        rejected.path(),
        "drafts/false.md",
        explicit_false.as_bytes(),
    );
    let tree = discover_content_tree(rejected.path(), ContentTreeLimits::default())
        .expect("filesystem discovery must preserve authored draft policy");
    let errors = validation_error_contract(tree.validate().expect_err("draft=false must fail"));
    assert!(has_error(
        &errors,
        "drafts/false.md",
        ContentValidationCode::DraftDirectoryConflict,
    ));

    let posts = new_root();
    let explicit_draft = post_source(FIRST_ID, "post-draft", Some(true));
    write(
        posts.path(),
        "posts/explicit-draft.md",
        explicit_draft.as_bytes(),
    );
    let validated = discover_content_tree(posts.path(), ContentTreeLimits::default())
        .expect("post-tree draft must be discovered")
        .validate()
        .expect("draft=true below posts must be valid");
    assert_eq!(validated.posts()[0].metadata().draft(), DraftStatus::Draft);
}

#[test]
fn rejects_unexpected_non_markdown_post_entries() {
    let root = new_root();
    write(root.path(), "posts/readme.txt", b"not a post");
    write(root.path(), "posts/upper.MD", b"not an exact extension");
    write(root.path(), "drafts/no-extension", b"not a draft post");

    let errors = error_contract(discover_content_tree(
        root.path(),
        ContentTreeLimits::default(),
    ));
    for path in ["drafts/no-extension", "posts/readme.txt", "posts/upper.MD"] {
        assert!(has_error(
            &errors,
            path,
            ContentValidationCode::UnexpectedPostEntry,
        ));
    }
}

#[test]
fn root_symlink_swap_after_open_remains_pinned_to_one_tree() {
    let parent = tempdir().expect("deployment fixture root must be created");
    let first = parent.path().join("first");
    let second = parent.path().join("second");
    fs::create_dir(&first).expect("first deployment must be created");
    fs::create_dir(&second).expect("second deployment must be created");
    write(&first, "publication.toml", PUBLICATION.as_bytes());
    write(&first, "assets/version.bin", b"first");
    write(&second, "publication.toml", PUBLICATION.as_bytes());
    write(&second, "assets/version.bin", b"second");

    let current = parent.path().join("current");
    let replacement = parent.path().join("replacement");
    symlink(&first, &current).expect("initial deployment symlink must be created");
    symlink(&second, &replacement).expect("replacement deployment symlink must be created");

    let tree = discover_content_tree_with_hook(&current, ContentTreeLimits::default(), || {
        fs::rename(&replacement, &current).expect("deployment symlink must switch atomically");
    })
    .expect("an opened root descriptor must pin the original deployment");

    assert_eq!(tree.assets()[0].bytes(), b"first");
    assert_eq!(
        fs::read(current.join("assets/version.bin")).expect("current deployment must be readable"),
        b"second"
    );
    let next = discover_content_tree(&current, ContentTreeLimits::default())
        .expect("the next discovery must use the new root-link target");
    assert_eq!(next.assets()[0].bytes(), b"second");
}

#[test]
fn regular_file_replaced_by_external_symlink_is_never_read() {
    let root = new_root();
    let external = tempdir().expect("external fixture root must be created");
    let race = write(root.path(), "assets/race.bin", b"inside");
    let outside = write(external.path(), "outside.bin", b"outside sentinel");
    let displaced = root.path().join("assets/displaced.bin");

    let errors = error_contract(discover_content_tree_with_hook(
        root.path(),
        ContentTreeLimits::default(),
        || {
            fs::rename(&race, &displaced).expect("original file must be displaced");
            symlink(&outside, &race).expect("racing symlink must be installed");
        },
    ));
    assert!(has_error(
        &errors,
        "assets/race.bin",
        ContentValidationCode::ContentSymlinkUnsupported,
    ));
}

#[test]
fn file_growth_after_open_is_caught_by_the_bounded_read() {
    let root = new_root();
    let path = write(root.path(), "assets/growing.bin", b"four");
    let mut grew = false;
    let errors = error_contract(discover_content_tree_with_hooks(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            1,
            4,
            (PUBLICATION.len() + 4) as u64,
            3,
            2,
            128,
        ),
        || {},
        |logical| {
            if logical == "assets/growing.bin" && !grew {
                use std::io::Write as _;
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .expect("growing fixture must open");
                file.write_all(b"x").expect("growing fixture must append");
                grew = true;
            }
        },
        |_| {},
    ));
    assert!(has_error(
        &errors,
        "assets/growing.bin",
        ContentValidationCode::ContentFileTooLarge,
    ));
}

#[test]
fn file_changed_after_its_read_is_rejected_by_the_final_stability_pass() {
    let root = new_root();
    let first = write(root.path(), "assets/a.bin", b"first");
    write(root.path(), "assets/b.bin", b"second");
    let mut changed = false;

    let errors = error_contract(discover_content_tree_with_hooks(
        root.path(),
        ContentTreeLimits::default(),
        || {},
        |_| {},
        |logical| {
            if logical == "assets/a.bin" && !changed {
                fs::write(&first, b"changed after read")
                    .expect("already-read fixture must be changed");
                changed = true;
            }
        },
    ));
    assert!(has_error(
        &errors,
        "assets/a.bin",
        ContentValidationCode::ContentEntryChanged,
    ));
}

#[test]
fn discovered_tree_remains_owned_after_the_source_tree_is_removed() {
    let root = new_root();
    let post = post_source(FIRST_ID, "owned", None);
    write(root.path(), "posts/owned.md", post.as_bytes());
    write(root.path(), "assets/owned.bin", b"owned bytes");
    let tree = discover_content_tree(root.path(), ContentTreeLimits::default())
        .expect("owned tree fixture must load");

    drop(root);

    assert_eq!(tree.assets()[0].bytes(), b"owned bytes");
    assert_eq!(
        tree.validate()
            .expect("owned source must validate after removal")
            .posts()[0]
            .metadata()
            .slug()
            .as_str(),
        "owned"
    );
}

#[test]
fn safe_resolver_errno_classification_is_typed_and_exhaustive_for_fixed_calls() {
    for error in [Errno::NOSYS, Errno::INVAL, Errno::TOOBIG] {
        assert_eq!(
            linux::classify_open_error(error, false).0,
            ContentValidationCode::ContentPlatformUnsupported,
        );
    }
    assert_eq!(
        linux::classify_open_error(Errno::LOOP, false).0,
        ContentValidationCode::ContentSymlinkUnsupported,
    );
    assert_eq!(
        linux::classify_open_error(Errno::XDEV, false).0,
        ContentValidationCode::UnsupportedContentEntryKind,
    );
    assert_eq!(
        linux::classify_open_error(Errno::IO, false).0,
        ContentValidationCode::ContentEntryUnreadable,
    );
}

#[test]
fn hardlinks_are_counted_once_per_logical_path() {
    let root = new_root();
    let bytes = b"hardlink";
    let first = write(root.path(), "assets/first.bin", bytes);
    fs::hard_link(&first, root.path().join("assets/second.bin"))
        .expect("hardlink fixture must be created");
    let exact_if_counted_twice = PUBLICATION.len() + (2 * bytes.len());

    let errors = error_contract(discover_content_tree(
        root.path(),
        limits(
            PUBLICATION.len() as u64,
            1,
            bytes.len() as u64,
            (exact_if_counted_twice - 1) as u64,
            8,
            4,
            128,
        ),
    ));
    assert!(
        errors
            .iter()
            .any(|error| { error.code == ContentValidationCode::ContentTreeTooLarge })
    );
}

#[test]
fn error_order_is_independent_of_filesystem_creation_order() {
    fn invalid_root(reverse: bool) -> TempDir {
        let root = new_root();
        let mut fixtures = vec![
            ("assets/z.svg", b"svg".as_slice()),
            ("assets/a%2e.bin", b"percent".as_slice()),
            ("assets/Photo.bin", b"first".as_slice()),
            ("assets/photo.BIN", b"second".as_slice()),
            ("posts/not-markdown.txt", b"text".as_slice()),
        ];
        if reverse {
            fixtures.reverse();
        }
        for (path, bytes) in fixtures {
            write(root.path(), path, bytes);
        }
        root
    }

    let first = invalid_root(false);
    let second = invalid_root(true);
    let forward = error_contract(discover_content_tree(
        first.path(),
        ContentTreeLimits::default(),
    ));
    let reverse = error_contract(discover_content_tree(
        second.path(),
        ContentTreeLimits::default(),
    ));
    assert_eq!(forward, reverse);
}

#[test]
fn valid_output_order_is_independent_of_filesystem_creation_order() {
    fn valid_root(reverse: bool) -> TempDir {
        let root = new_root();
        let first = post_source(FIRST_ID, "first", None);
        let second = post_source(SECOND_ID, "second", None);
        let mut fixtures = vec![
            ("posts/z-second.md", second.into_bytes()),
            ("assets/z.bin", b"z".to_vec()),
            ("posts/a-first.md", first.into_bytes()),
            ("assets/a.bin", b"a".to_vec()),
        ];
        if reverse {
            fixtures.reverse();
        }
        for (path, bytes) in fixtures {
            write(root.path(), path, &bytes);
        }
        root
    }

    let forward_root = valid_root(false);
    let reverse_root = valid_root(true);
    let forward = discover_content_tree(forward_root.path(), ContentTreeLimits::default())
        .expect("forward fixture must load");
    let reverse = discover_content_tree(reverse_root.path(), ContentTreeLimits::default())
        .expect("reverse fixture must load");

    assert_eq!(
        forward
            .posts()
            .iter()
            .map(|post| post.path().as_str())
            .collect::<Vec<_>>(),
        reverse
            .posts()
            .iter()
            .map(|post| post.path().as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        forward
            .assets()
            .iter()
            .map(|asset| asset.path().as_str())
            .collect::<Vec<_>>(),
        reverse
            .assets()
            .iter()
            .map(|asset| asset.path().as_str())
            .collect::<Vec<_>>()
    );
}
