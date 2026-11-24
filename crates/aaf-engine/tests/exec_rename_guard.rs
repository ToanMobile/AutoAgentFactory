#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(target_os = "macos")]
//! A write-deny on what is inside a writable directory holds only while the
//! directory itself cannot be renamed into another writable place and back.
//! The real `sandbox-exec` with a stand-in `claude` that tries exactly that
//! on every writable root that has something closed in it, and on the
//! directories of the owner's PATH that lie inside a writable root.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::ClaudeCodeRunner;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

/// `(marker, writable root below the world, closed file below that root)`.
const ROOTS: [(&str, &str, &str); 8] = [
    ("gradle", "home/.gradle", "init.d/evil.gradle"),
    ("cargo", "home/.cargo", "bin/tool"),
    ("android", "home/.android", "adbkey"),
    ("m2", "home/.m2", "settings.xml"),
    ("pub", "home/.pub-cache", "bin/tool"),
    ("sdk", "sdk", "platform-tools/adb"),
    ("account", "acct", "settings.json"),
    ("worktree", "scratch/nest/wt", ".git"),
];

const KEPT: &str = "kept\n";

/// `rename(2)` itself: `mv` onto an existing directory moves into it, which
/// is an ordinary write inside the root and not what is being tried.
const RENAME: &str =
    "ren() { /usr/bin/perl -e 'exit(rename($ARGV[0], $ARGV[1]) ? 0 : 1)' \"$1\" \"$2\"; }\n";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// Every root exists and holds its closed file. `scratch` stands in for
/// `/tmp`: writable, and the place roots are moved to. The worktree is two
/// levels inside it, so that the directories on the way down matter.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let world = World {
        root: tmp.path().canonicalize().unwrap(),
        _tmp: tmp,
    };
    for (_, root, closed) in ROOTS {
        let file = world.path(root).join(closed);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, KEPT).unwrap();
    }
    fs::create_dir_all(world.path("acct/hooks")).unwrap();
    world
}

fn run_with_path(world: &World, path: &str, script: &str) -> String {
    let claude = world.path("claude");
    write_executable(&claude, format!("#!/bin/sh\n{script}"));
    let text = |rel: &str| world.path(rel).to_string_lossy().into_owned();
    let env = ExecEnv {
        home: Some(text("home")),
        temp_dirs: vec![text("scratch")],
        sdk_dirs: vec![text("sdk")],
        parent_env: vec![("PATH".to_string(), path.to_string())],
        ..ExecEnv::default()
    };
    ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("test-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: text("scratch/nest/wt"),
            account: Some(AccountProfile {
                id: "claude_code-test".to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: text("acct"),
                name: "test".to_string(),
            }),
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
        })
        .unwrap()
        .log_tail
}

fn run(world: &World, script: &str) -> String {
    run_with_path(world, "/usr/bin:/bin", script)
}

fn sandbox_exists() -> bool {
    Path::new("/usr/bin/sandbox-exec").exists()
}

#[test]
fn a_writable_root_cannot_be_moved_out_and_back_to_reach_what_it_closes() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let scratch = world.path("scratch");
    let mut script = String::new();
    for (marker, root, closed) in ROOTS {
        // Out, rewrite, back: each step alone is a write to an open place
        // unless the root's own entry is closed.
        script.push_str(&format!(
            "mv '{root}' '{out}' 2>/dev/null && echo {marker}-moved\n\
             ( echo forged > '{out}/{closed}' ) 2>/dev/null && echo {marker}-written\n\
             mv '{out}' '{root}' 2>/dev/null\n",
            root = world.path(root).display(),
            out = scratch.join(format!("out-{marker}")).display(),
        ));
    }
    // A directory between a writable root and the worktree takes the
    // worktree along when it moves.
    script.push_str(&format!(
        "mv '{0}/nest' '{0}/nest2' 2>/dev/null && echo nest-moved\n\
         ( echo forged > '{0}/nest2/wt/.git' ) 2>/dev/null && echo nest-written\n\
         mv '{0}/nest2' '{0}/nest' 2>/dev/null\n\
         echo tried\n",
        scratch.display()
    ));

    let log = run(&world, &script);

    assert!(log.contains("tried"), "{log}");
    for marker in ROOTS.iter().map(|(marker, ..)| *marker).chain(["nest"]) {
        assert!(!log.contains(&format!("{marker}-moved")), "{log}");
        assert!(!log.contains(&format!("{marker}-written")), "{log}");
    }
    for (marker, root, closed) in ROOTS {
        let file = world.path(root).join(closed);
        assert_eq!(fs::read_to_string(file).unwrap(), KEPT, "{marker}: {log}");
    }
}

#[test]
fn a_root_that_does_not_exist_cannot_be_brought_in_with_closed_files_in_it() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    // No Maven and no Dart on this machine yet.
    fs::remove_dir_all(world.path("home/.m2")).unwrap();
    fs::remove_dir_all(world.path("home/.pub-cache")).unwrap();
    let script = format!(
        "{RENAME}\
         mkdir -p '{scratch}/m' '{scratch}/p/bin'\n\
         echo forged > '{scratch}/m/settings.xml'\n\
         echo forged > '{scratch}/p/bin/tool'\n\
         ren '{scratch}/m' '{home}/.m2' && echo m2-brought-in\n\
         ren '{scratch}/p' '{home}/.pub-cache' && echo pub-brought-in\n\
         mkdir -p '{home}/.m2/repository' && echo jar > '{home}/.m2/repository/x' && echo m2-used\n\
         echo tried\n",
        scratch = world.path("scratch").display(),
        home = world.path("home").display(),
    );

    let log = run(&world, &script);

    assert!(log.contains("tried"), "{log}");
    assert!(!log.contains("m2-brought-in"), "{log}");
    assert!(!log.contains("pub-brought-in"), "{log}");
    assert!(!world.path("home/.m2/settings.xml").exists());
    assert!(!world.path("home/.pub-cache/bin/tool").exists());
    // The first build that needs the cache still gets one.
    assert!(log.contains("m2-used"), "{log}");
}

#[test]
fn builds_still_create_rename_and_delete_inside_the_guarded_roots() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let mut script = String::new();
    for (marker, root, _) in ROOTS {
        // What a build does to its cache: `mkdir -p` of the cache itself,
        // new directories and files in it, atomic replace by rename, delete.
        script.push_str(&format!(
            "mkdir -p '{root}' && mkdir -p '{root}/new/dir' && echo data > '{root}/new/dir/f.tmp' \
             && mv '{root}/new/dir/f.tmp' '{root}/new/dir/f' && mv '{root}/new/dir' '{root}/new/dir2' \
             && echo top > '{root}/top-level-file' && rm '{root}/top-level-file' \
             && echo {marker}-built\n",
            root = world.path(root).display(),
        ));
    }

    let log = run(&world, &script);

    for (marker, root, _) in ROOTS {
        assert!(log.contains(&format!("{marker}-built")), "{marker}: {log}");
        assert_eq!(
            fs::read_to_string(world.path(root).join("new/dir2/f")).unwrap(),
            "data\n"
        );
        assert!(!world.path(root).join("top-level-file").exists());
    }
}

#[test]
fn a_hard_link_is_no_way_around_a_closed_file() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let script = format!(
        "ln '{closed}' '{scratch}/link' 2>/dev/null && echo linked-out\n\
         test -e '{scratch}/link' && ( echo forged >> '{scratch}/link' ) 2>/dev/null \
         && echo written-through\n\
         echo mine > '{scratch}/mine'\n\
         ln '{scratch}/mine' '{initd}/evil.gradle.kts' 2>/dev/null && echo linked-in\n\
         echo tried\n",
        closed = world.path("home/.m2/settings.xml").display(),
        initd = world.path("home/.gradle/init.d").display(),
        scratch = world.path("scratch").display(),
    );

    let log = run(&world, &script);

    assert!(log.contains("tried"), "{log}");
    assert!(!log.contains("linked-out"), "{log}");
    assert!(!log.contains("written-through"), "{log}");
    assert!(!log.contains("linked-in"), "{log}");
    assert_eq!(
        fs::read_to_string(world.path("home/.m2/settings.xml")).unwrap(),
        KEPT
    );
    assert!(!world.path("home/.gradle/init.d/evil.gradle.kts").exists());
}

/// The SDK directories a real session had on PATH before `/usr/bin`:
/// `emulator` and `cmdline-tools/latest/bin`, the second through a link.
fn sdk_on_path(world: &World) -> String {
    fs::create_dir_all(world.path("sdk/emulator")).unwrap();
    fs::create_dir_all(world.path("sdk/cmdline-tools/11.0/bin")).unwrap();
    fs::create_dir_all(world.path("sdk/platforms")).unwrap();
    symlink("11.0", world.path("sdk/cmdline-tools/latest")).unwrap();
    format!(
        "{0}/emulator:{0}/cmdline-tools/latest/bin:{0}/not-installed/bin:relative/bin::/usr/bin:/bin",
        world.path("sdk").display()
    )
}

#[test]
fn path_directories_inside_a_writable_root_are_closed() {
    if !sandbox_exists() {
        return;
    }
    let world = world();
    let path = sdk_on_path(&world);
    let sdk = world.path("sdk");
    let scratch = world.path("scratch");
    let mut script = String::new();
    // A `git` here is what `Command::new("git")` outside the sandbox runs.
    for (marker, dir) in [
        ("emulator", "emulator"),
        ("latest", "cmdline-tools/latest/bin"),
        ("resolved", "cmdline-tools/11.0/bin"),
    ] {
        script.push_str(&format!(
            "( echo '#!/bin/sh' > '{}/git' ) 2>/dev/null && echo {marker}-planted\n",
            sdk.join(dir).display()
        ));
    }
    script.push_str(&format!(
        "{RENAME}\
         mkdir -p '{sdk}/not-installed/bin' 2>/dev/null && echo missing-made\n\
         mv '{sdk}/emulator' '{scratch}/e' 2>/dev/null && echo emulator-moved\n\
         mv '{sdk}/cmdline-tools' '{scratch}/c' 2>/dev/null && echo tools-moved\n\
         mv '{sdk}/cmdline-tools/11.0' '{scratch}/v' 2>/dev/null && echo version-moved\n\
         rm '{sdk}/cmdline-tools/latest' 2>/dev/null && echo link-removed\n\
         mkdir -p '{scratch}/l/bin' && ren '{scratch}/l' '{sdk}/cmdline-tools/latest' \
         && echo link-replaced\n\
         mv '{sdk}' '{scratch}/s' 2>/dev/null && echo sdk-moved\n\
         echo platform > '{sdk}/platforms/android-36' && echo platforms-ok\n\
         echo tried\n",
        sdk = sdk.display(),
        scratch = scratch.display(),
    ));

    let log = run_with_path(&world, &path, &script);

    assert!(log.contains("tried"), "{log}");
    for marker in [
        "emulator-planted",
        "latest-planted",
        "resolved-planted",
        "missing-made",
        "emulator-moved",
        "tools-moved",
        "version-moved",
        "link-removed",
        "link-replaced",
        "sdk-moved",
    ] {
        assert!(!log.contains(marker), "{marker} in: {log}");
    }
    // The rest of the SDK is where builds install platforms.
    assert!(log.contains("platforms-ok"), "{log}");
    assert!(!sdk.join("emulator/git").exists());
    assert!(!sdk.join("cmdline-tools/11.0/bin/git").exists());
    assert!(
        fs::symlink_metadata(sdk.join("cmdline-tools/latest"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn a_path_directory_outside_every_writable_root_adds_no_rule() {
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let text = |rel: &str| root.join(rel).to_string_lossy().into_owned();
    fs::create_dir_all(root.join("open/bin")).unwrap();
    fs::create_dir_all(root.join("elsewhere/bin")).unwrap();
    let extra = |path_dirs: Vec<String>| SandboxPaths {
        writable: vec![text("open")],
        path_dirs,
        ..SandboxPaths::default()
    };
    let policy = SandboxPolicy {
        enabled: true,
        ..SandboxPolicy::default()
    };

    let without = seatbelt_profile(&policy, &extra(Vec::new())).unwrap();
    let outside = seatbelt_profile(
        &policy,
        &extra(vec![
            text("elsewhere/bin"),
            "relative/bin".to_string(),
            String::new(),
        ]),
    )
    .unwrap();
    let inside = seatbelt_profile(&policy, &extra(vec![text("open/bin")])).unwrap();

    assert_eq!(without, outside);
    assert!(
        inside.contains(&format!(
            "(deny file-write* (subpath \"{}\"))",
            text("open/bin")
        )),
        "{inside}"
    );
    // The root holds something closed now, so its own entry is closed too.
    assert!(
        inside.contains(&format!(
            "(deny file-write* (literal \"{}\"))",
            text("open")
        )),
        "{inside}"
    );
    assert!(!without.contains("(deny file-write* (literal"), "{without}");
}

/// Not a check: prints which directories of the real PATH the profile of
/// this machine would close. Reads the environment, writes nothing.
#[test]
#[ignore = "reads the real PATH and home directory"]
fn show_which_real_path_directories_would_be_closed() {
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
    let env = ExecEnv::from_process();
    let home = env.home.clone().unwrap();
    let mut writable = env.temp_dirs.clone();
    for cache in [
        ".cargo",
        ".rustup",
        ".gradle",
        ".npm",
        ".yarn",
        ".pub-cache",
    ] {
        writable.push(format!("{home}/{cache}"));
    }
    writable.extend(env.sdk_dirs.iter().cloned());
    let path_dirs: Vec<String> = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
    let policy = SandboxPolicy {
        enabled: true,
        ..SandboxPolicy::default()
    };
    let extra = SandboxPaths {
        writable,
        path_dirs: path_dirs.clone(),
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&policy, &extra).unwrap();
    println!("{} PATH entries", path_dirs.len());
    for line in profile.lines().filter(|line| line.starts_with("(deny")) {
        println!("{line}");
    }
}
