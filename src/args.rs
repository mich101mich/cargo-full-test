use std::{borrow::Cow, ffi::OsStr, path::PathBuf};

macro_rules! args {
    ( $( $arg:expr ),* ) => {
        {
            let mut args = crate::args::Args(Vec::new());
            $(
                args.append(crate::args::ArgLike::from($arg));
            )*
            args
        }
    };
}
pub(crate) use args;

macro_rules! cmd {
    ( cargo +stable $( $arg:expr )* ) => {
        Instruction::command("cargo", args!("+stable", $( $arg ),* ))
    };
    ( cargo +nightly $( $arg:expr )* ) => {
        Instruction::command("cargo", args!("+nightly", $( $arg ),* ))
    };
    ( cargo +{$version:expr} $( $arg:expr )* ) => {
        Instruction::command("cargo", args!(format!("+{}", $version), $( $arg ),* ))
    };
    ( rustup $( $arg:expr )* ) => {
        Instruction::command("rustup", args!( $( $arg ),* ))
    };
    ( coverage +stable --out-dir {$dir:ident / $cov_name:literal} $( $arg:expr )* ) => {
        {
            let out_dir = $dir.join($cov_name);
            fs_err::create_dir_all(&out_dir)?;
            let out_file = out_dir.join("lcov.info");
            Instruction::command("cargo", args!("+stable", "llvm-cov", "test", "--lcov", "--output-path", out_file, $( $arg ),*))
        }
    };
    ( coverage +nightly --out-dir {$dir:ident / $cov_name:literal} $( $arg:expr )* ) => {
        {
            let out_dir = $dir.join($cov_name);
            fs_err::create_dir_all(&out_dir)?;
            let out_file = out_dir.join("lcov.info");
            Instruction::command("cargo", args!("+nightly", "llvm-cov", "test", "--lcov", "--output-path", out_file, $( $arg ),*))
        }
    };
}
pub(crate) use cmd;

macro_rules! set {
    ( $key:ident = $value:literal ) => {
        Instruction::set_env(stringify!($key), $value)
    };
}
pub(crate) use set;

pub enum Instruction {
    Command {
        program: &'static str,
        args: Args,
    },
    SetEnv {
        key: &'static str,
        value: &'static str,
    },
    TakeMutex,
    ReleaseMutex,
}
impl Instruction {
    pub fn command(program: &'static str, args: Args) -> Self {
        Self::Command { program, args }
    }
    pub fn set_env(key: &'static str, value: &'static str) -> Self {
        Self::SetEnv { key, value }
    }
}

pub enum Arg {
    Str(Cow<'static, str>),
    Path(PathBuf),
}
pub struct Args(pub Vec<Arg>);

pub enum ArgLike {
    Arg(Arg),
    Args(Args),
}

impl Args {
    pub fn write_to(&self, s: &mut String, base_dir: &PathBuf) {
        for arg in &self.0 {
            s.push(' ');
            match arg {
                Arg::Str(st) => s.push_str(st),
                Arg::Path(p) => {
                    if let Ok(rel) = p.strip_prefix(base_dir) {
                        s.push_str(&rel.to_string_lossy());
                    } else {
                        s.push_str(&p.to_string_lossy());
                    }
                }
            }
        }
    }

    pub fn append(&mut self, arg: ArgLike) {
        match arg {
            ArgLike::Arg(a) => self.0.push(a),
            ArgLike::Args(args) => self.0.extend(args.0),
        }
    }
}

impl IntoIterator for Args {
    type Item = Arg;
    type IntoIter = std::vec::IntoIter<Arg>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl AsRef<OsStr> for Arg {
    fn as_ref(&self) -> &OsStr {
        match self {
            Arg::Str(s) => s.as_ref().as_ref(),
            Arg::Path(p) => p.as_os_str(),
        }
    }
}

impl From<&'static str> for ArgLike {
    fn from(s: &'static str) -> Self {
        ArgLike::Arg(Arg::Str(Cow::Borrowed(s)))
    }
}

impl From<String> for ArgLike {
    fn from(s: String) -> Self {
        ArgLike::Arg(Arg::Str(Cow::Owned(s)))
    }
}

impl<'a> From<&'a String> for ArgLike {
    fn from(s: &'a String) -> Self {
        ArgLike::Arg(Arg::Str(Cow::Owned(s.clone())))
    }
}
impl<'a> From<Cow<'a, str>> for ArgLike {
    fn from(s: Cow<'a, str>) -> Self {
        ArgLike::Arg(Arg::Str(Cow::Owned(s.into())))
    }
}

impl From<PathBuf> for ArgLike {
    fn from(p: PathBuf) -> Self {
        ArgLike::Arg(Arg::Path(p))
    }
}

// to allow flattening with `..args`
impl From<std::ops::RangeTo<Args>> for ArgLike {
    fn from(range: std::ops::RangeTo<Args>) -> Self {
        ArgLike::Args(range.end)
    }
}
