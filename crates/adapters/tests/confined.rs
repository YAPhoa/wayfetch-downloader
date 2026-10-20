//! Confined helper runs (architecture brief section 9.3), with this test
//! program as the launcher: like a host's `main`, its own `main` calls
//! `enter_if_asked` first, so it has its own small harness.
//!
//! Shell scripts show what a confined run can and cannot reach; 7-Zip
//! itself is tested too when `WAYFETCH_TEST_7ZIP` names it (7-Zip 25.01
//! or newer), and skipped otherwise.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::time::Duration;

use wayfetch_adapters::confine::{self, Confinement};
use wayfetch_adapters::plugin::{Check, Describe, Limits, Program, RunError};
use wayfetch_adapters::sevenzip::{MEMORY_BYTES, SevenZip};
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::extract::{Archive, EntryKind, ExtractError, Extractor, Halt, Layout};
use wayfetch_engine::protocol::media::PluginRole;

type Test = fn() -> Pin<Box<dyn Future<Output = ()>>>;

fn main() -> ExitCode {
    confine::enter_if_asked();
    let tests: &[(&str, Test)] = &[
        ("a_run_reaches_only_what_it_is_given", || {
            Box::pin(a_run_reaches_only_what_it_is_given())
        }),
        ("a_run_has_no_network_and_bounded_memory", || {
            Box::pin(a_run_has_no_network_and_bounded_memory())
        }),
        ("nothing_a_run_starts_outlives_it", || {
            Box::pin(nothing_a_run_starts_outlives_it())
        }),
        ("seven_zip_unpacks_confined", || {
            Box::pin(seven_zip_unpacks_confined())
        }),
        ("a_protected_archive_needs_its_password", || {
            Box::pin(a_protected_archive_needs_its_password())
        }),
        ("a_compressed_tar_unpacks_through_a_pipe", || {
            Box::pin(a_compressed_tar_unpacks_through_a_pipe())
        }),
    ];
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut failed = 0;
    for (name, test) in tests {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let passed =
            std::panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(test()))).is_ok();
        println!("test {name} ... {}", if passed { "ok" } else { "FAILED" });
        failed += usize::from(!passed);
    }
    if failed > 0 {
        println!("{failed} failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

const DESCRIBE: Describe = Describe {
    id: "script",
    name: "Script",
    role: PluginRole::ArchiveExtractor,
};

const LIMITS: Limits = Limits {
    deadline: Duration::from_secs(60),
    stdout_bytes: 64 * 1024,
};

/// A shell script in `dir` running `body`, checked confined.
async fn script(dir: &Path, body: &str) -> Program {
    let path = dir.join("helper");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let check = Check {
        args: &["version"],
        version: |_| None,
        env: Vec::new(),
        confinement: Some(Confinement::new(MEMORY_BYTES)),
    };
    let program = Program::detect(DESCRIBE, Some(path), &check).await;
    assert!(program.ready(), "{:?}", program.problem());
    program
}

/// What a run of `program` with `args` under `confinement` printed, or
/// its failure.
async fn run(
    program: &Program,
    args: &[&Path],
    confinement: &Confinement,
) -> Result<String, String> {
    run_until(program, args, confinement, std::future::pending()).await
}

/// The same, stopped when `stop` resolves.
async fn run_until(
    program: &Program,
    args: &[&Path],
    confinement: &Confinement,
    stop: impl Future<Output = ()>,
) -> Result<String, String> {
    match program
        .run_with(args, None, Some(confinement), LIMITS, stop)
        .await
    {
        Ok(out) => Ok(String::from_utf8(out.stdout).unwrap()),
        Err(RunError::Failed { stderr, .. }) => Err(stderr),
        Err(other) => Err(other.to_string()),
    }
}

async fn a_run_reaches_only_what_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let (given, secret, output, elsewhere) = (
        dir.path().join("given.txt"),
        dir.path().join("secret.txt"),
        dir.path().join("out"),
        dir.path().join("elsewhere"),
    );
    std::fs::write(&given, "given").unwrap();
    std::fs::write(&secret, "secret").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let modified = std::fs::metadata(&given).unwrap().modified().unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let helper = script(
        dir.path(),
        r#"case "$1" in
  read) cat "$2" ;;
  write) printf x > "$2/made" && mkdir "$2/d" && printf y > "$2/d/f" && printf ok ;;
  truncate) : > "$2" ;;
  link) ln -s /etc/passwd "$2/link" ;;
  chmod) chmod 777 "$2" ;;
  chown) chown "$(id -u)" "$2" ;;
  touch) touch -d 2000-01-01 "$2" ;;
  env) printf '%s|%s' "${HOME-unset}" "$LC_ALL" ;;
  *) printf '%s' "$(pwd)" ;;
esac"#,
    )
    .await;
    let confinement = Confinement::new(MEMORY_BYTES)
        .read([given.clone()])
        .write(&output, 1 << 20);
    let arg = |a: &str| PathBuf::from(a);
    assert_eq!(
        run(&helper, &[&arg("read"), &given], &confinement)
            .await
            .unwrap(),
        "given"
    );
    assert_eq!(
        run(&helper, &[&arg("write"), &output], &confinement)
            .await
            .unwrap(),
        "ok"
    );
    assert_eq!(std::fs::read(output.join("d/f")).unwrap(), b"y");
    // It starts in the folder it writes in, with an empty environment.
    assert_eq!(
        run(&helper, &[&arg("where")], &confinement).await.unwrap(),
        output.to_str().unwrap()
    );
    assert_eq!(
        run(&helper, &[&arg("env")], &confinement).await.unwrap(),
        "unset|C.UTF-8"
    );
    // Everything else is refused.
    for (what, target) in [
        ("read", &secret),
        ("write", &elsewhere),
        ("truncate", &given),
        ("link", &output),
        ("read", &PathBuf::from("/etc/passwd")),
    ] {
        let refused = run(&helper, &[&arg(what), target], &confinement).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("Permission denied")),
            "{what} {}: {refused:?}",
            target.display()
        );
    }
    assert_eq!(std::fs::read(&given).unwrap(), b"given");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

    // Changing a file's permissions, owner, or times seems to succeed but
    // changes nothing, not even in its own folder.
    let made = output.join("made");
    let (made_mode, made_modified) = {
        let made = std::fs::metadata(&made).unwrap();
        (made.permissions().mode(), made.modified().unwrap())
    };
    for (what, target) in [
        ("chmod", &secret),
        ("chmod", &made),
        ("chown", &secret),
        ("touch", &given),
        ("touch", &made),
    ] {
        let ignored = run(&helper, &[&arg(what), target], &confinement).await;
        assert_eq!(ignored.as_deref(), Ok(""), "{what} {}", target.display());
    }
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode(&secret) & 0o7777, 0o600);
    assert_eq!(mode(&made), made_mode);
    let modified_at = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
    assert_eq!(modified_at(&given), modified);
    assert_eq!(modified_at(&made), made_modified);
}

async fn nothing_a_run_starts_outlives_it() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    std::fs::create_dir(&output).unwrap();
    let helper = script(
        dir.path(),
        r#"case "$1" in
  spawn)
    ( while :; do printf x >> "$2/grew"; sleep 0.01; done ) </dev/null >/dev/null 2>&1 &
    [ "$3" = stay ] && sleep 60
    printf ok ;;
  escape) setsid sh -c 'printf x > "$1/escaped"' sh "$2" ;;
esac"#,
    )
    .await;
    let confinement = Confinement::new(MEMORY_BYTES).write(&output, 1 << 20);
    let grew = output.join("grew");
    // Whether the file it grows still grows.
    let growing = || async {
        let before = std::fs::metadata(&grew).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::metadata(&grew).map(|m| m.len()).unwrap_or(0) != before
    };
    let path = |a: &str| PathBuf::from(a);

    // Stopped, it returns once neither it nor what it started runs.
    let stopped = run_until(
        &helper,
        &[&path("spawn"), &output, &path("stay")],
        &confinement,
        tokio::time::sleep(Duration::from_millis(300)),
    )
    .await;
    assert_eq!(stopped, Err(RunError::Stopped.to_string()));
    assert!(std::fs::metadata(&grew).unwrap().len() > 0);
    assert!(!growing().await);

    // Finished, what it left running is stopped too.
    std::fs::remove_file(&grew).unwrap();
    let finished = run(&helper, &[&path("spawn"), &output], &confinement).await;
    assert_eq!(finished.as_deref(), Ok("ok"));
    assert!(!growing().await);

    // And nothing it starts can leave its process group.
    if Path::new("/usr/bin/setsid").is_file() {
        let escaped = run(&helper, &[&path("escape"), &output], &confinement).await;
        assert!(
            escaped
                .as_ref()
                .is_err_and(|e| e.contains("Permission denied")),
            "{escaped:?}"
        );
        assert!(!output.join("escaped").exists());
    }
}

async fn a_run_has_no_network_and_bounded_memory() {
    let Some(python) = ["/usr/bin/python3", "/bin/python3"]
        .iter()
        .find(|p| Path::new(p).is_file())
    else {
        println!("skipped: no python3 to open sockets with");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let helper = script(
        dir.path(),
        &format!(
            r#"exec {python} -I -c '
import socket, sys
what = sys.argv[1]
if what == "socket":
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
elif what == "unix":
    socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
elif what == "memory":
    bytearray(int(sys.argv[2]))
print("done")
' "$@""#
        ),
    )
    .await;
    let confinement = Confinement::new(1 << 30);
    for kind in ["socket", "unix"] {
        let refused = run(&helper, &[Path::new(kind)], &confinement).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("PermissionError")),
            "{kind}: {refused:?}"
        );
    }
    let small = run(
        &helper,
        &[Path::new("memory"), Path::new("1000000")],
        &confinement,
    )
    .await;
    assert_eq!(small.as_deref(), Ok("done\n"));
    let large = run(
        &helper,
        &[Path::new("memory"), Path::new(&(2u64 << 30).to_string())],
        &confinement,
    )
    .await;
    assert!(
        large.as_ref().is_err_and(|e| e.contains("MemoryError")),
        "{large:?}"
    );
}

/// 7-Zip, if the variable names one.
async fn seven_zip() -> Option<SevenZip> {
    let Some(path) = std::env::var_os("WAYFETCH_TEST_7ZIP") else {
        println!("skipped: WAYFETCH_TEST_7ZIP is not set");
        return None;
    };
    let zip = SevenZip::detect(Some(path.into())).await;
    assert!(
        Extractor::plugin(&zip).usable(),
        "{:?}",
        Extractor::plugin(&zip)
    );
    Some(zip)
}

/// A halt that never comes.
fn never() -> Halt {
    Box::pin(std::future::pending())
}

/// Runs 7-Zip with `args` in `dir`, unconfined, to make test archives.
fn make(dir: &Path, args: &[&str]) {
    let path = std::env::var_os("WAYFETCH_TEST_7ZIP").unwrap();
    let status = std::process::Command::new(path)
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

fn archive_at<'a>(
    path: &'a Path,
    volumes: &'a [PathBuf],
    password: Option<&'a Secret>,
) -> Archive<'a> {
    Archive {
        path,
        volumes,
        layout: Layout::Direct,
        password,
    }
}

async fn seven_zip_unpacks_confined() {
    let Some(zip) = seven_zip().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::write(src.join("été.txt"), "hello").unwrap();
    std::fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o750)).unwrap();
    // Incompressible, so the archive spans several volumes.
    let noise: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(src.join("d/noise.bin"), &noise).unwrap();
    make(
        &src,
        &["a", "-v100k", "-mx0", "../set.7z", "été.txt", "run.sh", "d"],
    );
    let first = dir.path().join("set.7z.001");
    let mut volumes: Vec<PathBuf> = (2..)
        .map(|n| dir.path().join(format!("set.7z.{n:03}")))
        .take_while(|v| v.is_file())
        .collect();
    assert!(volumes.len() >= 2, "{volumes:?}");

    let entries = zip
        .list(archive_at(&first, &volumes, None), never())
        .await
        .unwrap();
    let mut names: Vec<_> = entries.iter().map(|e| (e.path.as_str(), e.kind)).collect();
    names.sort_unstable_by_key(|(path, _)| *path);
    assert_eq!(
        names,
        [
            ("d", EntryKind::Directory),
            ("d/noise.bin", EntryKind::File),
            ("run.sh", EntryKind::File),
            ("été.txt", EntryKind::File),
        ]
    );
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    zip.extract(archive_at(&first, &volumes, None), &out, 1 << 30, never())
        .await
        .unwrap();
    assert_eq!(std::fs::read(out.join("d/noise.bin")).unwrap(), noise);
    assert_eq!(std::fs::read(out.join("été.txt")).unwrap(), b"hello");
    // 7-Zip 26 gives a file its stored permissions as it creates it;
    // earlier releases set them afterwards, which confinement skips.
    let mode = std::fs::metadata(out.join("run.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let version = Extractor::plugin(&zip).version.unwrap();
    if version.as_str() >= "26" {
        assert_eq!(mode, 0o750, "{version}");
    } else {
        assert_eq!(mode & 0o111, 0, "{version}: {mode:o}");
    }

    // A volume it is not given cannot be read, even beside the others.
    let last = volumes.pop().unwrap();
    let partial = dir.path().join("partial");
    std::fs::create_dir(&partial).unwrap();
    let refused = zip
        .extract(
            archive_at(&first, &volumes, None),
            &partial,
            1 << 30,
            never(),
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");

    // No file may grow past the budget it is given.
    let small = dir.path().join("small");
    std::fs::create_dir(&small).unwrap();
    volumes.push(last);
    let stopped = zip
        .extract(archive_at(&first, &volumes, None), &small, 100_000, never())
        .await;
    assert!(
        matches!(stopped, Err(ExtractError::Failed(_))),
        "{stopped:?}"
    );
}

async fn a_protected_archive_needs_its_password() {
    let Some(zip) = seven_zip().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::write(src.join("a.txt"), "hello").unwrap();
    std::fs::write(src.join("d/b.txt"), "world").unwrap();
    make(
        &src,
        &["a", "-psecret word", "-mhe=on", "../p.7z", "a.txt", "d"],
    );
    let path = dir.path().join("p.7z");
    assert_eq!(
        zip.list(archive_at(&path, &[], None), never()).await,
        Err(ExtractError::NeedsPassword)
    );
    let wrong = Secret::from_text("nope");
    assert_eq!(
        zip.list(archive_at(&path, &[], Some(&wrong)), never())
            .await,
        Err(ExtractError::WrongPassword)
    );
    let right = Secret::from_text("secret word");
    let entries = zip
        .list(archive_at(&path, &[], Some(&right)), never())
        .await
        .unwrap();
    let mut names: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.txt", "d", "d/b.txt"]);
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    zip.extract(archive_at(&path, &[], Some(&right)), &out, 1 << 20, never())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(out.join("d/b.txt")).unwrap(),
        "world"
    );
}

async fn a_compressed_tar_unpacks_through_a_pipe() {
    let Some(zip) = seven_zip().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), "hello").unwrap();
    make(&src, &["a", "-ttar", "../t.tar", "a.txt"]);
    make(dir.path(), &["a", "-tgzip", "t.tar.gz", "t.tar"]);
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let tar = Archive {
        path: &dir.path().join("t.tar.gz"),
        volumes: &[],
        layout: Layout::CompressedTar,
        password: None,
    };
    zip.extract(tar, &out, 1 << 20, never()).await.unwrap();
    assert_eq!(std::fs::read_to_string(out.join("a.txt")).unwrap(), "hello");
    // A damaged stream is reported, not unpacked.
    std::fs::write(dir.path().join("bad.tar.gz"), b"\x1f\x8b\x08\x00junk").unwrap();
    let bad = Archive {
        path: &dir.path().join("bad.tar.gz"),
        ..tar
    };
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(zip.extract(bad, &empty, 1 << 20, never()).await.is_err());
}
