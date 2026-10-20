//! Confining a helper program that reads untrusted input, such as 7-Zip
//! unpacking a downloaded archive (architecture brief section 9.3).
//!
//! A confined run sees only what it is given: the files it may read, one
//! folder it may write in, and the system's program files. It cannot open
//! a socket, change any file's permissions, owner, times, or extended
//! attributes, or leave its process group; it gets an empty environment,
//! runs under memory, file size, and core dump limits, and is killed if
//! Wayfetch exits. On Linux, Landlock confines its files (ABI 3, Linux
//! 6.2, so it cannot truncate files it may only read) and, from Linux
//! 6.12, signals to other processes; seccomp refuses the rest, and
//! resource limits bound it. The restrictions bind the program and
//! anything it starts. It inherits no open file but its
//! standard input and output, since every file Wayfetch opens is closed
//! when a program starts.
//!
//! The restrictions are entered by a launcher: the host's own program,
//! started again with [`LAUNCH`] before its arguments, which restricts
//! itself and then becomes the helper. So no code runs between creating
//! the child process and starting the helper, and the host needs no
//! unsafe code. Each host calls [`enter_if_asked`] first thing in `main`.
//!
//! Where a restriction cannot be enforced, such as on an older kernel, a
//! container that forbids Landlock, another system, or a host that is no
//! launcher, the run fails and says why: a confined program never runs
//! unconfined.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The argument that starts a host program as the launcher.
pub const LAUNCH: &str = "--wayfetch-confined-run";

/// The program that launches confined runs, once known.
static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();

/// Called first thing in a host's `main`: if the program was started as
/// the launcher, confines itself and becomes the helper, never returning;
/// otherwise records the program as the launcher of later runs.
pub fn enter_if_asked() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(LAUNCH)) {
        if let Ok(program) = std::env::current_exe() {
            use_launcher(program);
        }
        return;
    }
    let why = match launch(args) {
        Ok(never) => match never {},
        Err(why) => why,
    };
    eprintln!("ERROR: {why}");
    std::process::exit(126);
}

/// Uses `program`, which calls [`enter_if_asked`] in its `main`, to launch
/// confined runs, unless a launcher is already known.
pub fn use_launcher(program: PathBuf) {
    let _ = LAUNCHER.set(program);
}

/// Confines this process as its arguments say, then starts the helper in
/// its place.
fn launch(args: impl Iterator<Item = OsString>) -> Result<std::convert::Infallible, String> {
    let (confinement, program, args) = Confinement::from_args(args)?;
    imp::restrict(&confinement, &program)?;
    imp::exec(&program, args)
}

/// What a confined run may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confinement {
    /// Files it may read.
    read: Vec<PathBuf>,
    /// The folder it may write in, if any.
    write: Option<PathBuf>,
    /// The most address space it may map, in bytes.
    memory_bytes: u64,
    /// The largest file it may write, in bytes.
    file_bytes: u64,
}

impl Confinement {
    /// A run that may read no files and write nowhere, with at most
    /// `memory_bytes` of memory.
    pub fn new(memory_bytes: u64) -> Self {
        Self {
            read: Vec::new(),
            write: None,
            memory_bytes,
            file_bytes: 0,
        }
    }

    /// The run may also read `files`.
    pub fn read(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
        self.read.extend(files);
        self
    }

    /// The run may create, write, and remove files and folders inside
    /// `folder`, none larger than `file_bytes`, and starts there.
    pub fn write(mut self, folder: &Path, file_bytes: u64) -> Self {
        self.write = Some(folder.to_owned());
        self.file_bytes = file_bytes;
        self
    }

    /// A command running `program` confined, through the launcher, with
    /// an empty environment; or why there is none.
    pub(crate) fn command(&self, program: &Path) -> Result<tokio::process::Command, String> {
        let launcher = LAUNCHER
            .get()
            .ok_or("Wayfetch cannot confine helper programs in this program")?;
        let mut command = tokio::process::Command::new(launcher);
        command
            .arg(LAUNCH)
            .args(self.to_args())
            .arg("--")
            .arg(program)
            .env_clear()
            // 7-Zip reads and writes names as UTF-8 under this locale.
            .env("LC_ALL", "C.UTF-8")
            .current_dir(self.write.as_deref().unwrap_or(Path::new("/")));
        Ok(command)
    }

    /// The launcher's arguments that describe it.
    fn to_args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        for file in &self.read {
            args.extend(["--read".into(), file.into()]);
        }
        if let Some(folder) = &self.write {
            args.extend(["--write".into(), folder.into()]);
        }
        args.extend([
            "--memory".into(),
            self.memory_bytes.to_string().into(),
            "--file-bytes".into(),
            self.file_bytes.to_string().into(),
        ]);
        args
    }

    /// The confinement, program, and program arguments the launcher was
    /// given.
    fn from_args(
        mut args: impl Iterator<Item = OsString>,
    ) -> Result<(Self, PathBuf, Vec<OsString>), String> {
        let mut confinement = Self::new(0);
        let number = |value: Option<OsString>| {
            value
                .and_then(|v| v.into_string().ok())
                .and_then(|v| v.parse().ok())
                .ok_or("a limit is not a number")
        };
        loop {
            let arg = args.next().ok_or("no program was named")?;
            match arg.to_str() {
                Some("--read") => confinement
                    .read
                    .push(args.next().ok_or("--read needs a file")?.into()),
                Some("--write") => {
                    confinement.write = Some(args.next().ok_or("--write needs a folder")?.into());
                }
                Some("--memory") => confinement.memory_bytes = number(args.next())?,
                Some("--file-bytes") => confinement.file_bytes = number(args.next())?,
                Some("--") => break,
                _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
            }
        }
        let program = args.next().ok_or("no program was named")?;
        Ok((confinement, program.into(), args.collect()))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;

    use landlock::{
        ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, Scope,
    };
    use rustix::process::{Resource, Rlimit, Signal, set_parent_process_death_signal, setrlimit};
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    use super::Confinement;

    /// The Landlock version required: the first that also stops a run
    /// truncating files it may only read.
    const ABI_REQUIRED: ABI = ABI::V3;

    /// Where the system's programs and libraries are: readable and
    /// runnable, so a program built against shared libraries, or started
    /// through a shell script, still starts.
    const SYSTEM: &[&str] = &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin"];

    /// System calls a confined run may not make:
    /// - sockets, so no network, and io_uring, which could open them
    ///   indirectly;
    /// - changes to a file's permissions, owner, times, or extended
    ///   attributes, which Landlock does not confine, so no file outside
    ///   the run's folder can be changed that way (7-Zip sets a file's
    ///   permissions as it creates it, so it loses only stored times);
    /// - leaving its process group, so stopping the group stops every
    ///   process the run started.
    const REFUSED: &[i64] = &[
        libc::SYS_socket,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        SYS_FCHMODAT2,
        libc::SYS_fchown,
        libc::SYS_fchownat,
        libc::SYS_utimensat,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        SYS_SETXATTRAT,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        SYS_REMOVEXATTRAT,
        libc::SYS_setsid,
        libc::SYS_setpgid,
    ];

    /// The same, by their older names that only some architectures have.
    #[cfg(target_arch = "x86_64")]
    const REFUSED_LEGACY: &[i64] = &[
        libc::SYS_chmod,
        libc::SYS_chown,
        libc::SYS_lchown,
        libc::SYS_utime,
        libc::SYS_utimes,
        libc::SYS_futimesat,
    ];
    #[cfg(not(target_arch = "x86_64"))]
    const REFUSED_LEGACY: &[i64] = &[];

    // System calls added since Linux 5.1 share one number on every
    // architecture; these are not yet in `libc` for all of them.
    const SYS_FCHMODAT2: i64 = 452;
    const SYS_SETXATTRAT: i64 = 463;
    const SYS_REMOVEXATTRAT: i64 = 466;

    /// `ioctl` requests a confined run may not make: changing a file's
    /// attribute flags (`chattr`), `FS_IOC_SETFLAGS` in both widths and
    /// `FS_IOC_FSSETXATTR`.
    const REFUSED_IOCTLS: &[u64] = &[0x4008_6602, 0x4004_6602, 0x401c_5820];

    /// Restricts this process, which runs only the launcher, as
    /// `confinement` says, before it starts `program`.
    pub(super) fn restrict(confinement: &Confinement, program: &Path) -> Result<(), String> {
        // Stopped if Wayfetch is, rather than left writing to a folder the
        // next start removes. The signal follows the thread that started
        // the run, which in an async runtime lives as long as it does.
        set_parent_process_death_signal(Some(Signal::KILL))
            .map_err(|e| format!("a run cannot be tied to Wayfetch: {e}"))?;
        let ruleset = ruleset(confinement, program)
            .map_err(|e| format!("Landlock cannot confine helper programs here: {e}"))?;
        let filter =
            filter().map_err(|e| format!("seccomp cannot confine helper programs: {e}"))?;
        for (resource, value) in [
            (Resource::As, confinement.memory_bytes),
            (Resource::Fsize, confinement.file_bytes),
            (Resource::Core, 0),
        ] {
            let limit = Rlimit {
                current: Some(value),
                maximum: Some(value),
            };
            setrlimit(resource, limit).map_err(|e| format!("a limit cannot be set: {e}"))?;
        }
        ruleset
            .restrict_self()
            .map_err(|e| format!("Landlock cannot confine helper programs here: {e}"))?;
        seccompiler::apply_filter(&filter)
            .map_err(|e| format!("seccomp cannot confine helper programs: {e}"))
    }

    /// Starts `program` in place of this process.
    pub(super) fn exec(
        program: &Path,
        args: Vec<OsString>,
    ) -> Result<std::convert::Infallible, String> {
        let error = std::process::Command::new(program).args(args).exec();
        Err(format!("{} cannot be started: {error}", program.display()))
    }

    /// The Landlock rules: everything is refused but what the run is
    /// given, and the system's program files.
    fn ruleset(
        confinement: &Confinement,
        program: &Path,
    ) -> Result<landlock::RulesetCreated, Box<dyn std::error::Error>> {
        let abi = ABI_REQUIRED;
        let run = AccessFs::from_read(abi);
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(abi))?
            // No signals to processes outside the run, where the kernel
            // can refuse them (Linux 6.12).
            .set_compatibility(CompatLevel::BestEffort)
            .scope(Scope::Signal)?
            .create()?
            .set_compatibility(CompatLevel::HardRequirement);
        for dir in SYSTEM.iter().map(Path::new).filter(|d| d.is_dir()) {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, run))?;
        }
        // The program itself, wherever it is installed.
        ruleset = ruleset.add_rule(PathBeneath::new(
            PathFd::new(program.canonicalize()?)?,
            AccessFs::from_file(abi) & run,
        ))?;
        ruleset = ruleset.add_rule(PathBeneath::new(
            PathFd::new("/dev/null")?,
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))?;
        for file in &confinement.read {
            // The file itself, never one a link put in its place.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                .open(file)?;
            ruleset = ruleset.add_rule(PathBeneath::new(file, AccessFs::ReadFile))?;
        }
        if let Some(folder) = &confinement.write {
            // Plain files and folders only: no links, devices, pipes, or
            // sockets, nothing run from there, and nothing moved out.
            let refused: BitFlags<AccessFs> = AccessFs::MakeSym
                | AccessFs::MakeSock
                | AccessFs::MakeFifo
                | AccessFs::MakeBlock
                | AccessFs::MakeChar
                | AccessFs::Execute
                | AccessFs::Refer;
            ruleset = ruleset.add_rule(PathBeneath::new(
                PathFd::new(folder)?,
                AccessFs::from_all(abi) & !refused,
            ))?;
        }
        Ok(ruleset)
    }

    /// The seccomp filter refusing [`REFUSED`] and [`REFUSED_IOCTLS`]
    /// with `EACCES`.
    fn filter() -> Result<BpfProgram, seccompiler::BackendError> {
        let mut rules = REFUSED
            .iter()
            .chain(REFUSED_LEGACY)
            .map(|&call| (call, Vec::new()))
            .collect::<BTreeMap<_, _>>();
        let ioctls = REFUSED_IOCTLS
            .iter()
            .map(|&request| {
                let condition =
                    SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, request)?;
                SeccompRule::new(vec![condition])
            })
            .collect::<Result<_, _>>()?;
        rules.insert(libc::SYS_ioctl, ioctls);
        SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EACCES.unsigned_abs()),
            TargetArch::try_from(std::env::consts::ARCH)?,
        )?
        .try_into()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::ffi::OsString;
    use std::path::Path;

    use super::Confinement;

    pub(super) fn restrict(_: &Confinement, _: &Path) -> Result<(), String> {
        Err("only Linux can confine helper programs so far".into())
    }

    pub(super) fn exec(_: &Path, _: Vec<OsString>) -> Result<std::convert::Infallible, String> {
        Err("only Linux can confine helper programs so far".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confinement_travels_to_the_launcher_whole() {
        let confinement = Confinement::new(4 << 30)
            .read([
                PathBuf::from("/r/a b.part1.rar"),
                PathBuf::from("/r/--write"),
            ])
            .write(Path::new("/r/.wayfetch/extract/4-g2"), 1 << 20);
        let mut args = confinement.to_args();
        args.extend(["--".into(), "/usr/bin/7zz".into(), "x".into(), "--".into()]);
        let (back, program, rest) = Confinement::from_args(args.into_iter()).unwrap();
        assert_eq!(back, confinement);
        assert_eq!(program, Path::new("/usr/bin/7zz"));
        assert_eq!(rest, ["x", "--"]);
        assert!(
            Confinement::from_args(
                ["--memory", "lots", "--", "p"]
                    .map(OsString::from)
                    .into_iter()
            )
            .is_err()
        );
        assert!(Confinement::from_args(["--read"].map(OsString::from).into_iter()).is_err());
    }
}
