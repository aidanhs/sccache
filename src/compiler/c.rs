// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use compiler::{Cacheable, ColorMode, Compiler, CompilerArguments, CompileCommand, CompilerHasher, CompilerKind,
               Compilation, HashResult};
use dist;
use futures::{Future, future};
use futures_cpupool::CpuPool;
use mock_command::CommandCreatorSync;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::hash::Hash;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use tar;
use util::{HashToDigest, Digest};

use errors::*;

/// A generic implementation of the `Compiler` trait for C/C++ compilers.
#[derive(Clone)]
pub struct CCompiler<I>
    where I: CCompilerImpl,
{
    executable: PathBuf,
    executable_digest: String,
    compiler: I,
}

/// A generic implementation of the `CompilerHasher` trait for C/C++ compilers.
#[derive(Debug, Clone)]
pub struct CCompilerHasher<I>
    where I: CCompilerImpl,
{
    parsed_args: ParsedArguments,
    executable: PathBuf,
    executable_digest: String,
    compiler: I,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Language {
    C,
    Cxx,
    ObjectiveC,
    ObjectiveCxx,
}

/// The results of parsing a compiler commandline.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone)]
pub struct ParsedArguments {
    /// The input source file.
    pub input: PathBuf,
    /// The type of language used in the input source file.
    pub language: Language,
    /// The file in which to generate dependencies.
    pub depfile: Option<PathBuf>,
    /// Output files, keyed by a simple name, like "obj".
    pub outputs: HashMap<&'static str, PathBuf>,
    /// Commandline arguments for the preprocessor.
    pub preprocessor_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor or the compiler.
    pub common_args: Vec<OsString>,
    /// Whether or not the `-showIncludes` argument is passed on MSVC
    pub msvc_show_includes: bool,
    /// Whether the compilation is generating profiling data.
    pub profile_generate: bool,
}

impl ParsedArguments {
    pub fn output_pretty(&self) -> Cow<str> {
        self.outputs.get("obj")
            .and_then(|o| o.file_name())
            .map(|s| s.to_string_lossy())
            .unwrap_or(Cow::Borrowed("Unknown filename"))
    }
}

impl Language {
    pub fn from_file_name(file: &Path) -> Option<Self> {
        match file.extension().and_then(|e| e.to_str()) {
            Some("c") => Some(Language::C),
            Some("cc") | Some("cpp") | Some("cxx") => Some(Language::Cxx),
            Some("m") => Some(Language::ObjectiveC),
            Some("mm") => Some(Language::ObjectiveCxx),
            e => {
                trace!("Unknown source extension: {}", e.unwrap_or("(None)"));
                None
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match *self {
            Language::C => "c",
            Language::Cxx => "c++",
            Language::ObjectiveC => "objc",
            Language::ObjectiveCxx => "objc++",
        }
    }
}

/// A generic implementation of the `Compilation` trait for C/C++ compilers.
struct CCompilation<I: CCompilerImpl> {
    parsed_args: ParsedArguments,
    preprocessed_input: Vec<u8>,
    executable: PathBuf,
    compiler: I,
    cwd: PathBuf,
    env_vars: Vec<(OsString, OsString)>,
}

/// Supported C compilers.
#[derive(Debug, PartialEq, Clone)]
pub enum CCompilerKind {
    /// GCC
    GCC,
    /// clang
    Clang,
    /// Microsoft Visual C++
    MSVC,
}

/// An interface to a specific C compiler.
pub trait CCompilerImpl: Clone + fmt::Debug + Send + 'static {
    /// Return the kind of compiler.
    fn kind(&self) -> CCompilerKind;
    /// Determine whether `arguments` are supported by this compiler.
    fn parse_arguments(&self,
                       arguments: &[OsString],
                       cwd: &Path) -> CompilerArguments<ParsedArguments>;
    /// Run the C preprocessor with the specified set of arguments.
    fn preprocess<T>(&self,
                     creator: &T,
                     executable: &Path,
                     parsed_args: &ParsedArguments,
                     cwd: &Path,
                     env_vars: &[(OsString, OsString)])
                     -> SFuture<process::Output> where T: CommandCreatorSync;
    /// Generate a command that can be used to invoke the C compiler to perform
    /// the compilation.
    fn generate_compile_command(&self,
                                executable: &Path,
                                parsed_args: &ParsedArguments,
                                cwd: &Path,
                                env_vars: &[(OsString, OsString)])
                                -> Result<(CompileCommand, Cacheable)>;
}

impl <I> CCompiler<I>
    where I: CCompilerImpl,
{
    pub fn new(compiler: I, executable: PathBuf, pool: &CpuPool) -> SFuture<CCompiler<I>>
    {
        Box::new(Digest::file(executable.clone(), &pool).map(move |digest| {
            CCompiler {
                executable: executable,
                executable_digest: digest,
                compiler: compiler,
            }
        }))
    }
}

impl<T: CommandCreatorSync, I: CCompilerImpl> Compiler<T> for CCompiler<I> {
    fn kind(&self) -> CompilerKind { CompilerKind::C(self.compiler.kind()) }
    fn parse_arguments(&self,
                       arguments: &[OsString],
                       cwd: &Path) -> CompilerArguments<Box<CompilerHasher<T> + 'static>> {
        match self.compiler.parse_arguments(arguments, cwd) {
            CompilerArguments::Ok(args) => {
                CompilerArguments::Ok(Box::new(CCompilerHasher {
                    parsed_args: args,
                    executable: self.executable.clone(),
                    executable_digest: self.executable_digest.clone(),
                    compiler: self.compiler.clone(),
                }))
            }
            CompilerArguments::CannotCache(why) => CompilerArguments::CannotCache(why),
            CompilerArguments::NotCompilation => CompilerArguments::NotCompilation,
        }
    }

    fn box_clone(&self) -> Box<Compiler<T>> {
        Box::new((*self).clone())
    }
}

impl<T, I> CompilerHasher<T> for CCompilerHasher<I>
    where T: CommandCreatorSync,
          I: CCompilerImpl,
{
    fn generate_hash_key(self: Box<Self>,
                         daemon_client: Arc<dist::DaemonClientRequester>,
                         creator: &T,
                         cwd: PathBuf,
                         env_vars: Vec<(OsString, OsString)>,
                         pool: &CpuPool)
                         -> SFuture<HashResult<T>>
    {
        let me = *self;
        let CCompilerHasher { parsed_args, executable, executable_digest, compiler } = me;
        let result = compiler.preprocess(creator, &executable, &parsed_args, &cwd, &env_vars);
        let out_pretty = parsed_args.output_pretty().into_owned();
        let env_vars = env_vars.to_vec();
        let toolchain_pool = pool.clone();
        let result = result.map_err(move |e| {
            debug!("[{}]: preprocessor failed: {:?}", out_pretty, e);
            e
        });
        let out_pretty = parsed_args.output_pretty().into_owned();
        Box::new(result.or_else(move |err| {
            match err {
                Error(ErrorKind::ProcessError(output), _) => {
                    debug!("[{}]: preprocessor returned error status {:?}",
                           out_pretty,
                           output.status.code());
                    // Drop the stdout since it's the preprocessor output, just hand back stderr and
                    // the exit status.
                    bail!(ErrorKind::ProcessError(process::Output {
                        stdout: vec!(),
                        .. output
                    }))
                }
                e @ _ => Err(e),
            }
        }).and_then(move |preprocessor_result| {
            trace!("[{}]: Preprocessor output is {} bytes",
                   parsed_args.output_pretty(),
                   preprocessor_result.stdout.len());

            let key = {
                hash_key(&executable_digest,
                         parsed_args.language,
                         &parsed_args.common_args,
                         &env_vars,
                         &preprocessor_result.stdout)
            };
            // A compiler binary may be a symlink to another and so has the same digest, but that means
            // the toolchain will not contain the correct path to invoke the compiler! Add the compiler
            // executable path to try and prevent this
            let weak_toolchain_key = format!("{}-{}", executable.to_string_lossy(), executable_digest);
            // CPU pool futures are eager, delay until poll is called
            let env_executable = executable.clone();
            let toolchain_future = Box::new(future::lazy(move || {
                toolchain_pool.spawn_fn(move || {
                    let archive_id = daemon_client.put_toolchain_cache(&weak_toolchain_key, &mut move |f| {
                        info!("Packaging C compiler");
                        // TODO: write our own, since this is GPL
                        let curdir = env::current_dir().unwrap();
                        env::set_current_dir("/tmp").unwrap();
                        let output = process::Command::new("icecc-create-env").arg(&env_executable).output().unwrap();
                        if !output.status.success() {
                            println!("{:?}\n\n\n===========\n\n\n{:?}", output.stdout, output.stderr);
                            panic!("failed to create toolchain")
                        }
                        let file_line = output.stdout.split(|&b| b == b'\n').find(|line| line.starts_with(b"creating ")).unwrap();
                        let filename = &file_line[b"creating ".len()..];
                        let filename = OsStr::from_bytes(filename);
                        io::copy(&mut File::open(filename).unwrap(), &mut {f}).unwrap();
                        fs::remove_file(filename).unwrap();
                        env::set_current_dir(curdir).unwrap()
                    });
                    future::ok(dist::Toolchain {
                        docker_img: "aidanhs/busybox".to_owned(),
                        archive_id,
                    })
                })
            }));
            Ok(HashResult {
                key: key,
                compilation: Box::new(CCompilation {
                    parsed_args: parsed_args,
                    preprocessed_input: preprocessor_result.stdout,
                    executable: executable,
                    compiler: compiler,
                    cwd,
                    env_vars,
                }),
                dist_toolchain: toolchain_future,
            })
        }))
    }

    fn color_mode(&self) -> ColorMode {
        //TODO: actually implement this for C compilers
        ColorMode::Auto
    }

    fn output_pretty(&self) -> Cow<str>
    {
        self.parsed_args.output_pretty()
    }

    fn box_clone(&self) -> Box<CompilerHasher<T>>
    {
        Box::new((*self).clone())
    }
}

impl<T: CommandCreatorSync, I: CCompilerImpl> Compilation<T> for CCompilation<I> {
    fn generate_compile_command(&self)
                                -> Result<(CompileCommand, Cacheable)>
    {
        let CCompilation { ref parsed_args, ref executable, ref compiler, preprocessed_input: _, ref cwd, ref env_vars } = *self;
        compiler.generate_compile_command(executable, parsed_args, cwd, env_vars)
    }

    fn generate_dist_requests(&self,
                              toolchain: SFuture<dist::Toolchain>)
                              -> SFuture<(dist::JobAllocRequest, dist::JobRequest, Cacheable)> {

        // Unsure why this needs UFCS
        let (mut command, cacheable) = <Self as Compilation<T>>::generate_compile_command(self).unwrap();

        // https://gcc.gnu.org/onlinedocs/gcc-4.9.0/gcc/Overall-Options.html
        let language = match self.parsed_args.language {
            Language::C => "cpp-output",
            Language::Cxx => "c++-cpp-output",
            Language::ObjectiveC => "objective-c-cpp-output",
            Language::ObjectiveCxx => "objective-c++-cpp-output",
        };
        let mut lang_next = false;
        for arg in command.arguments.iter_mut() {
            if arg == "-x" {
                lang_next = true
            } else if lang_next {
                *arg = OsString::from(language);
                break
            }
        }

        let mut builder = tar::Builder::new(vec![]);
        let preprocessed_path = command.cwd.join(&self.parsed_args.input);
        let metadata = fs::metadata(&preprocessed_path).unwrap();
        let preprocessed_path = preprocessed_path.strip_prefix("/").unwrap();

        let mut file_header = tar::Header::new_ustar();
        file_header.set_metadata(&metadata);
        file_header.set_path(preprocessed_path).unwrap();
        file_header.set_size(self.preprocessed_input.len() as u64); // Metadata is non-preprocessed
        file_header.set_cksum();

        builder.append(&file_header, self.preprocessed_input.as_slice()).unwrap();
        let inputs_archive = builder.into_inner().unwrap();
        // Unsure why this needs UFCS
        let outputs = <Self as Compilation<T>>::outputs(self).map(|(_, p)| p.to_owned()).collect();

        Box::new(toolchain.map(move |toolchain| (
            dist::JobAllocRequest {
                toolchain: toolchain.clone(),
            },
            dist::JobRequest {
                command,
                inputs_archive,
                outputs,
                toolchain,
                toolchain_data: None,
            },
            cacheable
        )))
    }

    fn outputs<'a>(&'a self) -> Box<Iterator<Item=(&'a str, &'a Path)> + 'a>
    {
        Box::new(self.parsed_args.outputs.iter().map(|(k, v)| (*k, &**v)))
    }
}

/// The cache is versioned by the inputs to `hash_key`.
pub const CACHE_VERSION: &[u8] = b"6";

lazy_static! {
    /// Environment variables that are factored into the cache key.
    static ref CACHED_ENV_VARS: HashSet<&'static OsStr> = [
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
    ].iter().map(OsStr::new).collect();
}

/// Compute the hash key of `compiler` compiling `preprocessor_output` with `args`.
pub fn hash_key(compiler_digest: &str,
                language: Language,
                arguments: &[OsString],
                env_vars: &[(OsString, OsString)],
                preprocessor_output: &[u8]) -> String
{
    // If you change any of the inputs to the hash, you should change `CACHE_VERSION`.
    let mut m = Digest::new();
    m.update(compiler_digest.as_bytes());
    m.update(CACHE_VERSION);
    m.update(language.as_str().as_bytes());
    for arg in arguments {
        arg.hash(&mut HashToDigest { digest: &mut m });
    }
    for &(ref var, ref val) in env_vars.iter() {
        if CACHED_ENV_VARS.contains(var.as_os_str()) {
            var.hash(&mut HashToDigest { digest: &mut m });
            m.update(&b"="[..]);
            val.hash(&mut HashToDigest { digest: &mut m });
        }
    }
    m.update(preprocessor_output);
    m.finish()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_hash_key_executable_contents_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED : &'static [u8] = b"hello world";
        assert_neq!(hash_key("abcd", Language::C, &args, &[], &PREPROCESSED),
                    hash_key("wxyz", Language::C, &args, &[], &PREPROCESSED));
    }

    #[test]
    fn test_hash_key_args_differs() {
        let digest = "abcd";
        let abc = ovec!["a", "b", "c"];
        let xyz = ovec!["x", "y", "z"];
        let ab = ovec!["a", "b"];
        let a = ovec!["a"];
        const PREPROCESSED: &'static [u8] = b"hello world";
        assert_neq!(hash_key(digest, Language::C, &abc, &[], &PREPROCESSED),
                    hash_key(digest, Language::C, &xyz, &[], &PREPROCESSED));

        assert_neq!(hash_key(digest, Language::C, &abc, &[], &PREPROCESSED),
                    hash_key(digest, Language::C, &ab, &[], &PREPROCESSED));

        assert_neq!(hash_key(digest, Language::C, &abc, &[], &PREPROCESSED),
                    hash_key(digest, Language::C, &a, &[], &PREPROCESSED));
    }

    #[test]
    fn test_hash_key_preprocessed_content_differs() {
        let args = ovec!["a", "b", "c"];
        assert_neq!(hash_key("abcd", Language::C, &args, &[], &b"hello world"[..]),
                    hash_key("abcd", Language::C, &args, &[], &b"goodbye"[..]));
    }

    #[test]
    fn test_hash_key_env_var_differs() {
        let args = ovec!["a", "b", "c"];
        let digest = "abcd";
        const PREPROCESSED: &'static [u8] = b"hello world";
        for var in CACHED_ENV_VARS.iter() {
            let h1 = hash_key(digest, Language::C, &args, &[], &PREPROCESSED);
            let vars = vec![(OsString::from(var), OsString::from("something"))];
            let h2 = hash_key(digest, Language::C, &args, &vars, &PREPROCESSED);
            let vars = vec![(OsString::from(var), OsString::from("something else"))];
            let h3 = hash_key(digest, Language::C, &args, &vars, &PREPROCESSED);
            assert_neq!(h1, h2);
            assert_neq!(h2, h3);
        }
    }
}
