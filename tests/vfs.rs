use std::{collections::HashSet, fs, time::Duration};

// use flexi_logger::Logger;
use crossbeam_channel::{RecvTimeoutError, Receiver, unbounded};
use ra_vfs::{Vfs, VfsChange, RootEntry, Filter, RelativePath, VfsTask, Watch};
use tempfile::tempdir;

/// Processes exactly `num_tasks` events waiting in the `vfs` message queue.
///
/// Panics if there are not exactly that many tasks enqueued for processing.
fn process_tasks(vfs: &mut Vfs, task_receiver: &mut Receiver<VfsTask>, num_tasks: u32) {
    process_tasks_in_range(vfs, task_receiver, num_tasks, num_tasks);
}

/// Processes up to `max_count` events waiting in the `vfs` message queue.
///
/// Panics if it cannot process at least `min_count` events.
/// Panics if more than `max_count` events are enqueued for processing.
fn process_tasks_in_range(
    vfs: &mut Vfs,
    task_receiver: &mut Receiver<VfsTask>,
    min_count: u32,
    max_count: u32,
) {
    for i in 0..max_count {
        let task = match task_receiver.recv_timeout(Duration::from_secs(3)) {
            Err(RecvTimeoutError::Timeout) if i >= min_count => return,
            otherwise => otherwise.unwrap(),
        };
        log::debug!("{:?}", task);
        vfs.handle_task(task);
    }
    assert!(task_receiver.is_empty());
}

macro_rules! assert_match {
    ($x:expr, $pat:pat) => {
        assert_match!($x, $pat, ())
    };
    ($x:expr, $pat:pat, $assert:expr) => {
        match $x {
            $pat => $assert,
            x => assert!(false, "Expected {}, got {:?}", stringify!($pat), x),
        };
    };
}

struct IncludeRustFiles;

impl IncludeRustFiles {
    fn boxed() -> Box<Self> {
        Box::new(Self {})
    }
}

impl Filter for IncludeRustFiles {
    fn include_dir(&self, dir_path: &RelativePath) -> bool {
        const IGNORED_FOLDERS: &[&str] = &["node_modules", "target", ".git"];

        let is_ignored = dir_path.components().any(|c| IGNORED_FOLDERS.contains(&c.as_str()));

        let hidden = dir_path.file_stem().map(|s| s.starts_with(".")).unwrap_or(false);

        !is_ignored && !hidden
    }

    fn include_file(&self, file_path: &RelativePath) -> bool {
        file_path.extension() == Some("rs")
    }
}

fn task_chan() -> (Receiver<VfsTask>, Box<dyn FnMut(VfsTask) + Send>) {
    let (sender, receiver) = unbounded();
    (receiver, Box::new(move |task| sender.send(task).unwrap()))
}

#[test]
fn test_vfs_ignore() -> std::io::Result<()> {
    // flexi_logger::Logger::with_str("vfs=debug,ra_vfs=debug").start().unwrap();

    let files = [
        ("ignore_a/foo.rs", "hello"),
        ("ignore_a/bar.rs", "world"),
        ("ignore_a/b/baz.rs", "nested hello"),
        ("ignore_a/LICENSE", "extensionless file"),
        ("ignore_a/b/AUTHOR", "extensionless file"),
        ("ignore_a/.hidden.txt", "hidden file"),
        ("ignore_a/.hidden_folder/file.rs", "hidden folder containing rust file"),
        (
            "ignore_a/.hidden_folder/nested/foo.rs",
            "file inside nested folder inside a hidden folder",
        ),
        ("ignore_a/node_modules/module.js", "hidden file js"),
        ("ignore_a/node_modules/module2.rs", "node rust"),
        ("ignore_a/node_modules/nested/foo.bar", "hidden file bar"),
    ];

    let dir = tempdir().unwrap();
    for (path, text) in files.iter() {
        let file_path = dir.path().join(path);
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, text)?
    }

    let a_root = dir.path().join("ignore_a");
    let b_root = dir.path().join("ignore_a/b");

    let (mut task_receiver, cb) = task_chan();
    let (mut vfs, _) = Vfs::new(
        vec![
            RootEntry::new(a_root, IncludeRustFiles::boxed()),
            RootEntry::new(b_root, IncludeRustFiles::boxed()),
        ],
        cb,
        Watch(true),
    );
    process_tasks(&mut vfs, &mut task_receiver, 2);
    {
        let files = vfs
            .commit_changes()
            .into_iter()
            .flat_map(|change| {
                let files = match change {
                    VfsChange::AddRoot { files, .. } => files,
                    _ => panic!("unexpected change"),
                };
                files.into_iter().map(|(_id, path, text)| {
                    let text: String = (&*text).clone();
                    (format!("{}", path.display()), text)
                })
            })
            .collect::<HashSet<_>>();

        let expected_files = [("foo.rs", "hello"), ("bar.rs", "world"), ("baz.rs", "nested hello")]
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect::<HashSet<_>>();

        assert_eq!(files, expected_files);
    }

    // rust-analyzer#734: fsevents has a bunch of events still sitting around.
    process_tasks_in_range(
        &mut vfs,
        &mut task_receiver,
        0,
        if cfg!(target_os = "macos") { 7 } else { 0 },
    );
    assert!(vfs.commit_changes().is_empty());

    // These will get filtered out
    vfs.add_file_overlay(&dir.path().join("ignore_a/node_modules/spam.rs"), "spam".to_string());
    vfs.add_file_overlay(&dir.path().join("ignore_a/node_modules/spam2.rs"), "spam".to_string());
    vfs.add_file_overlay(&dir.path().join("ignore_a/node_modules/spam3.rs"), "spam".to_string());
    vfs.add_file_overlay(&dir.path().join("ignore_a/LICENSE2"), "text".to_string());
    assert_match!(vfs.commit_changes().as_slice(), []);

    fs::create_dir_all(dir.path().join("ignore_a/node_modules/sub1")).unwrap();
    fs::write(dir.path().join("ignore_a/node_modules/sub1/new.rs"), "new hello").unwrap();

    assert_match!(
        task_receiver.recv_timeout(Duration::from_millis(300)), // slightly more than watcher debounce delay
        Err(RecvTimeoutError::Timeout)
    );

    Ok(())
}

#[test]
fn test_vfs_works() -> std::io::Result<()> {
    // Logger::with_str("vfs=debug,ra_vfs=debug").start().unwrap();

    let files = [
        ("a/foo.rs", "hello"),
        ("a/bar.rs", "world"),
        ("a/b/baz.rs", "nested hello"),
        ("a/LICENSE", "extensionless file"),
        ("a/b/AUTHOR", "extensionless file"),
        ("a/.hidden.txt", "hidden file"),
    ];

    let dir = tempdir().unwrap();
    for (path, text) in files.iter() {
        let file_path = dir.path().join(path);
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, text)?
    }

    let a_root = dir.path().join("a");
    let b_root = dir.path().join("a/b");

    let (mut task_receiver, cb) = task_chan();
    let (mut vfs, _) = Vfs::new(
        vec![
            RootEntry::new(a_root, IncludeRustFiles::boxed()),
            RootEntry::new(b_root, IncludeRustFiles::boxed()),
        ],
        cb,
        Watch(true),
    );
    process_tasks(&mut vfs, &mut task_receiver, 2);
    {
        let files = vfs
            .commit_changes()
            .into_iter()
            .flat_map(|change| {
                let files = match change {
                    VfsChange::AddRoot { files, .. } => files,
                    _ => panic!("unexpected change"),
                };
                files.into_iter().map(|(_id, path, text)| {
                    let text: String = (&*text).clone();
                    (format!("{}", path.display()), text)
                })
            })
            .collect::<HashSet<_>>();

        let expected_files = [("foo.rs", "hello"), ("bar.rs", "world"), ("baz.rs", "nested hello")]
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect::<HashSet<_>>();

        assert_eq!(files, expected_files);
    }

    // rust-analyzer#734: fsevents has a bunch of events still sitting around.
    process_tasks_in_range(
        &mut vfs,
        &mut task_receiver,
        0,
        if cfg!(target_os = "macos") { 7 } else { 0 },
    );
    assert!(vfs.commit_changes().is_empty());

    fs::write(&dir.path().join("a/b/baz.rs"), "quux").unwrap();
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_match!(
        vfs.commit_changes().as_slice(),
        [VfsChange::ChangeFile { text, .. }],
        assert_eq!(text.as_str(), "quux")
    );

    vfs.add_file_overlay(&dir.path().join("a/b/baz.rs"), "m".to_string());
    assert_match!(
        vfs.commit_changes().as_slice(),
        [VfsChange::ChangeFile { text, .. }],
        assert_eq!(text.as_str(), "m")
    );

    // changing file on disk while overlayed doesn't generate a VfsChange
    fs::write(&dir.path().join("a/b/baz.rs"), "corge").unwrap();
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_match!(vfs.commit_changes().as_slice(), []);

    // removing overlay restores data on disk
    vfs.remove_file_overlay(&dir.path().join("a/b/baz.rs"));
    assert_match!(
        vfs.commit_changes().as_slice(),
        [VfsChange::ChangeFile { text, .. }],
        assert_eq!(text.as_str(), "corge")
    );

    vfs.add_file_overlay(&dir.path().join("a/b/spam.rs"), "spam".to_string());
    assert_match!(vfs.commit_changes().as_slice(), [VfsChange::AddFile { text, path, .. }], {
        assert_eq!(text.as_str(), "spam");
        assert_eq!(path, "spam.rs");
    });

    vfs.remove_file_overlay(&dir.path().join("a/b/spam.rs"));
    assert_match!(
        vfs.commit_changes().as_slice(),
        [VfsChange::RemoveFile { path, .. }],
        assert_eq!(path, "spam.rs")
    );

    fs::create_dir_all(dir.path().join("a/sub1/sub2")).unwrap();
    fs::write(dir.path().join("a/sub1/sub2/new.rs"), "new hello").unwrap();
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_match!(vfs.commit_changes().as_slice(), [VfsChange::AddFile { text, path, .. }], {
        assert_eq!(text.as_str(), "new hello");
        assert_eq!(path, "sub1/sub2/new.rs");
    });

    fs::rename(&dir.path().join("a/sub1/sub2/new.rs"), &dir.path().join("a/sub1/sub2/new1.rs"))
        .unwrap();

    // rust-analyzer#734: For testing purposes, work-around
    // passcod/notify#181 by processing either 1 or 2 events. (In
    // particular, Mac can hand back either 1 or 2 events in a
    // timing-dependent fashion.)
    //
    // rust-analyzer#827: Windows generates extra `Write` events when
    // renaming? meaning we have extra tasks to process.
    process_tasks_in_range(&mut vfs, &mut task_receiver, 1, if cfg!(windows) { 4 } else { 2 });
    match vfs.commit_changes().as_slice() {
        [VfsChange::RemoveFile { path: removed_path, .. }, VfsChange::AddFile { text, path: added_path, .. }] =>
        {
            assert_eq!(removed_path, "sub1/sub2/new.rs");
            assert_eq!(added_path, "sub1/sub2/new1.rs");
            assert_eq!(text.as_str(), "new hello");
        }

        // Hopefully passcod/notify#181 will be addressed in some
        // manner that will reliably emit an event mentioning
        // `sub1/sub2/new.rs`. But until then, must accept that
        // debouncing loses information unrecoverably.
        [VfsChange::AddFile { text, path: added_path, .. }] => {
            assert_eq!(added_path, "sub1/sub2/new1.rs");
            assert_eq!(text.as_str(), "new hello");
        }

        changes => panic!(
            "Expected events for rename of {OLD} to {NEW}, got: {GOT:?}",
            OLD = "sub1/sub2/new.rs",
            NEW = "sub1/sub2/new1.rs",
            GOT = changes
        ),
    }

    fs::remove_file(&dir.path().join("a/sub1/sub2/new1.rs")).unwrap();
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_match!(
        vfs.commit_changes().as_slice(),
        [VfsChange::RemoveFile { path, .. }],
        assert_eq!(path, "sub1/sub2/new1.rs")
    );

    {
        vfs.add_file_overlay(&dir.path().join("a/memfile.rs"), "memfile".to_string());
        assert_match!(
            vfs.commit_changes().as_slice(),
            [VfsChange::AddFile { text, .. }],
            assert_eq!(text.as_str(), "memfile")
        );
        fs::write(&dir.path().join("a/memfile.rs"), "ignore me").unwrap();
        process_tasks(&mut vfs, &mut task_receiver, 1);
        assert_match!(vfs.commit_changes().as_slice(), []);
    }

    // should be ignored
    fs::create_dir_all(dir.path().join("a/target")).unwrap();
    fs::write(&dir.path().join("a/target/new.rs"), "ignore me").unwrap();

    assert_match!(
        task_receiver.recv_timeout(Duration::from_millis(300)), // slightly more than watcher debounce delay
        Err(RecvTimeoutError::Timeout)
    );

    Ok(())
}

#[test]
fn test_disabled_watch() {
    let files = [("a/foo.rs", "hello"), ("a/bar.rs", "world")];

    let dir = tempdir().unwrap();
    for (path, text) in files.iter() {
        let file_path = dir.path().join(path);
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, text).unwrap();
    }

    let a_root = dir.path().join("a");

    let (mut task_receiver, cb) = task_chan();
    let (mut vfs, _) =
        Vfs::new(vec![RootEntry::new(a_root, IncludeRustFiles::boxed())], cb, Watch(false));
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_eq!(vfs.commit_changes().len(), 1);

    fs::write(dir.path().join("a/foo.rs"), "goodbye").unwrap();
    assert_match!(
        task_receiver.recv_timeout(Duration::from_millis(300)),
        Err(RecvTimeoutError::Timeout)
    );
    vfs.notify_changed(dir.path().join("a/foo.rs"));
    process_tasks(&mut vfs, &mut task_receiver, 1);
    assert_eq!(vfs.commit_changes().len(), 1);
}
